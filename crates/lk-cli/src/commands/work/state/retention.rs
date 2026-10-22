use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_vault::{MutationPlan, MutationSession};
use serde_json::Value;

use super::super::resources::Subject;
use super::{
    Consumption, Publication, Seal, StoredOutput, Work, fail, output_path, read_optional,
    require_id, require_settled, revision, work_path,
};

#[derive(Default)]
pub(crate) struct WorkRetention {
    pub removed: usize,
    pub retained: usize,
    pub bytes: usize,
    pub empty_directories: usize,
}

fn is_empty_work_directory(directory: &Path) -> miette::Result<bool> {
    for entry in std::fs::read_dir(directory).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        if entry.file_name() != "nodes" || !entry.file_type().map_err(fail)?.is_dir() {
            return Ok(false);
        }
        if let Some(entry) = std::fs::read_dir(entry.path()).map_err(fail)?.next() {
            entry.map_err(fail)?;
            return Ok(false);
        }
    }
    Ok(true)
}

struct RetentionCapture {
    id: String,
    revisions: BTreeMap<PathBuf, String>,
    bytes: usize,
    references: BTreeSet<String>,
    queue_subject: Option<(String, String)>,
    expired: bool,
}

#[derive(Default)]
struct QueueReferences {
    identities: BTreeSet<(String, String)>,
    snapshots: BTreeMap<PathBuf, String>,
}

impl QueueReferences {
    fn capture(root: &Path) -> miette::Result<Self> {
        let mut references = Self::default();
        for path in Self::files(root, Path::new(".lorekeeper/queue"), "jsonl")? {
            let full = lk_vault::page_path(root, &path).map_err(fail)?;
            Self::require_file(&full)?;
            let (bytes, pending) = lk_queue::read_tasks_snapshot(&full).map_err(fail)?;
            if !pending.is_rewritable() {
                return Err(fail("unparseable pending tasks prevent work retention"));
            }
            for task in pending.tasks {
                task.validate_retained_input().map_err(fail)?;
                task.generation_policy().map_err(fail)?;
                references
                    .identities
                    .insert((task.task_id, task.cache_hash));
            }
            references
                .snapshots
                .insert(path, String::from_utf8(bytes).map_err(fail)?);
        }
        for path in Self::files(root, Path::new(".lorekeeper/queue/results"), "json")? {
            let raw = read_optional(root, &path)?
                .ok_or_else(|| fail("staged result disappeared during work retention"))?;
            let name = path.to_string_lossy();
            let identity = if name.ends_with(lk_queue::SECTION_RESULT_SUFFIX) {
                let result: lk_queue::SectionResult = serde_json::from_str(&raw).map_err(fail)?;
                (result.task_id, result.cache_hash)
            } else if name.ends_with(lk_queue::SYNTHESIS_RESULT_SUFFIX) {
                let result: lk_queue::SynthesisResult = serde_json::from_str(&raw).map_err(fail)?;
                (result.task_id, result.cache_hash)
            } else {
                let result: lk_queue::TaskResult = serde_json::from_str(&raw).map_err(fail)?;
                (result.task_id, result.cache_hash)
            };
            if !references.identities.contains(&identity) {
                return Err(fail("unmatched staged results prevent work retention"));
            }
            references.snapshots.insert(path, raw);
        }
        Ok(references)
    }

    fn files(root: &Path, directory: &Path, extension: &str) -> miette::Result<Vec<PathBuf>> {
        let full = lk_vault::page_path(root, directory).map_err(fail)?;
        let entries = match std::fs::read_dir(full) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(fail(error)),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(fail)?;
            let path = directory.join(entry.file_name());
            if path.extension().is_some_and(|value| value == extension) {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    fn require_file(path: &Path) -> miette::Result<()> {
        let metadata = std::fs::symlink_metadata(path).map_err(fail)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(fail("work retention requires direct regular queue files"));
        }
        Ok(())
    }
}

fn capture_retention(root: &Path, id: &str, cutoff: i64) -> miette::Result<RetentionCapture> {
    let work = Work::load(root, id)?;
    let subject: Subject = serde_json::from_value(
        work.context
            .get("subject")
            .cloned()
            .ok_or_else(|| fail("work retention requires a known subject"))?,
    )
    .map_err(fail)?;
    let queue_subject = match &subject {
        Subject::Queue {
            task_id,
            cache_hash,
            ..
        } => Some((task_id.clone(), cache_hash.clone())),
        Subject::Audit { .. } => None,
    };
    let directory = work_path(id, "")?;
    let mut known = [
        "plan.json",
        "seal.json",
        "final.json",
        "assessment.json",
        "publication.json",
        "consumption.json",
    ]
    .into_iter()
    .map(|name| work_path(id, name))
    .collect::<miette::Result<BTreeSet<_>>>()?;
    let nodes = work.nodes()?;
    for node in &nodes {
        known.insert(output_path(id, &node.id)?);
    }
    let mut revisions = BTreeMap::new();
    let mut bytes = 0;
    let mut references = BTreeSet::new();
    let mut unknown = false;
    for entry in walkdir::WalkDir::new(root.join(&directory))
        .max_depth(2)
        .follow_links(false)
    {
        let entry = entry.map_err(fail)?;
        if entry.file_type().is_symlink() {
            return Err(fail("work retention refuses symlink artifacts"));
        }
        let path = entry.path().strip_prefix(root).map_err(fail)?.to_path_buf();
        if path == directory || (path == directory.join("nodes") && entry.file_type().is_dir()) {
            continue;
        }
        if !known.contains(&path) {
            unknown = true;
            continue;
        }
        let raw = read_optional(root, &path)?
            .ok_or_else(|| fail("work artifact disappeared during retention capture"))?;
        if path.parent() == Some(directory.join("nodes").as_path()) {
            let stored: StoredOutput = serde_json::from_str(&raw).map_err(fail)?;
            if stored.output.work_id != id
                || Some(stored.output.node_id.as_str())
                    != path.file_stem().and_then(|name| name.to_str())
                || revision(&stored.output)? != stored.output_revision
            {
                return Err(fail("invalid retained work observation"));
            }
            if let Some(reused) = stored.reused_from {
                require_id(&reused.work_id)?;
                require_id(&reused.node_id)?;
                references.insert(reused.work_id);
            }
        }
        bytes += raw.len();
        revisions.insert(path, lk_core::concept::file_revision(&raw));
    }
    let read = |name| -> miette::Result<Option<String>> {
        let path = work_path(id, name)?;
        match revisions.get(&path) {
            Some(expected) => read_captured(root, &path, expected).map(Some),
            None => Ok(None),
        }
    };
    let publication = read("publication.json")?;
    let consumption = read("consumption.json")?;
    if publication.is_some() && consumption.is_some() {
        return Err(fail(
            "work cannot be both published and consumed source observations",
        ));
    }
    let expired = if let Some(raw) = publication {
        let receipt: Publication = serde_json::from_str(&raw).map_err(fail)?;
        let seal: Seal = serde_json::from_str(
            &read("seal.json")?.ok_or_else(|| fail("published work seal is missing"))?,
        )
        .map_err(fail)?;
        let result: Value = serde_json::from_str(
            &read("final.json")?.ok_or_else(|| fail("published work result is missing"))?,
        )
        .map_err(fail)?;
        if receipt.work_id != id
            || seal.work_id != id
            || receipt.final_result_revision != seal.final_result_revision
            || revision(&result)? != receipt.final_result_revision
        {
            return Err(fail("published work receipt, seal or result differs"));
        }
        !unknown && receipt.published_at.as_second() < cutoff
    } else if let Some(raw) = consumption {
        let receipt: Consumption = serde_json::from_str(&raw).map_err(fail)?;
        let Subject::Queue {
            task_id,
            cache_hash,
            target,
            target_revision,
        } = &subject
        else {
            return Err(fail("consumed source work has no queue subject"));
        };
        let node = nodes.last().expect("nonempty work tree");
        let root_path = output_path(id, &node.id)?;
        let output: StoredOutput = serde_json::from_str(&read_captured(
            root,
            &root_path,
            revisions
                .get(&root_path)
                .ok_or_else(|| fail("consumed source root observation is missing"))?,
        )?)
        .map_err(fail)?;
        if work.context["purpose"]["extraction_scope"]["policy"] != "source-proposals-v1"
            || work.context["purpose"]["task_kind"] != "extract-concepts"
            || receipt.work_id != id
            || receipt.root_node != node.id
            || receipt.root_output_revision != output.output_revision
            || &receipt.task_id != task_id
            || &receipt.cache_hash != cache_hash
            || &receipt.target != target
            || &receipt.target_revision != target_revision
            || receipt.result_revision.trim().is_empty()
            || !output.output.unresolved.is_empty()
        {
            return Err(fail(
                "source consumption receipt or completed observation differs",
            ));
        }
        !unknown && receipt.consumed_at.as_second() < cutoff
    } else {
        false
    };
    Ok(RetentionCapture {
        id: id.to_owned(),
        revisions,
        bytes,
        references,
        queue_subject,
        expired,
    })
}

fn read_captured(root: &Path, path: &Path, expected: &str) -> miette::Result<String> {
    let raw = read_optional(root, path)?.ok_or_else(|| fail("work retention input disappeared"))?;
    if lk_core::concept::file_revision(&raw) != expected {
        return Err(fail("work retention input changed"));
    }
    Ok(raw)
}

/// Caller holds the workflow lease and mutation session when pruning. Unknown,
/// unconsumed/unpublished and referenced work survives; no source or knowledge file is owned here.
pub(crate) fn prune_published(
    root: &Path,
    session: Option<&MutationSession>,
    cutoff: i64,
) -> miette::Result<WorkRetention> {
    require_settled(root)?;
    let directory = lk_vault::page_path(root, Path::new(".lorekeeper/work")).map_err(fail)?;
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkRetention::default());
        }
        Err(error) => return Err(fail(error)),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(fail("work store must be a direct directory"));
    }
    let root_owned = root.canonicalize().map_err(fail)?;
    let root = root_owned.as_path();
    if session.is_some_and(|session| session.root() != root) {
        return Err(fail("retention session belongs to another vault"));
    }
    let queue_directory =
        lk_vault::page_path(root, Path::new(".lorekeeper/queue")).map_err(fail)?;
    let _queue = crate::commands::queue::claim_writer(&queue_directory, session.is_none())?;
    let queue = QueueReferences::capture(root)?;
    let mut captures = BTreeMap::new();
    let mut empty = Vec::new();
    for entry in std::fs::read_dir(directory).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| fail("work address is not UTF-8"))?;
        require_id(&id)?;
        if !entry.file_type().map_err(fail)?.is_dir() {
            return Err(fail("work entry must be a direct directory"));
        }
        if is_empty_work_directory(&entry.path())? {
            empty.push(id);
            continue;
        }
        captures.insert(id.clone(), capture_retention(root, &id, cutoff)?);
    }
    for work in captures.values() {
        for reference in &work.references {
            if !captures.contains_key(reference) {
                return Err(fail(
                    "referenced work is missing from the retention inventory",
                ));
            }
        }
    }
    let mut retained = captures
        .values()
        .filter(|work| {
            !work.expired
                || work
                    .queue_subject
                    .as_ref()
                    .is_some_and(|subject| queue.identities.contains(subject))
        })
        .map(|work| work.id.clone())
        .collect::<BTreeSet<_>>();
    let mut pending = retained.iter().cloned().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if let Some(work) = captures.get(&id) {
            for reference in &work.references {
                if retained.insert(reference.clone()) {
                    pending.push(reference.clone());
                }
            }
        }
    }
    let mut report = WorkRetention::default();
    for id in empty.into_iter().filter(|id| !retained.contains(id)) {
        report.empty_directories += 1;
        if session.is_some() {
            for suffix in ["nodes", ""] {
                match std::fs::remove_dir(root.join(work_path(&id, suffix)?)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(fail(error)),
                }
            }
        }
    }
    let mut mutation = MutationPlan::default();
    for (path, raw) in &queue.snapshots {
        mutation.require_unchanged(path, raw.clone());
    }
    for work in captures.values() {
        if retained.contains(&work.id) {
            report.retained += 1;
            for (path, digest) in &work.revisions {
                mutation.require_unchanged(path, read_captured(root, path, digest)?);
            }
            continue;
        }
        report.removed += 1;
        report.bytes += work.bytes;
        if session.is_some() {
            let current = capture_retention(root, &work.id, cutoff)?;
            if current.revisions != work.revisions || !current.expired {
                return Err(fail("work retention inventory changed"));
            }
            for (path, digest) in &work.revisions {
                mutation.remove(path, read_captured(root, path, digest)?);
            }
        }
    }
    if let Some(session) = session
        && report.removed > 0
    {
        session.apply(mutation).map_err(fail)?;
        for work in captures
            .values()
            .filter(|work| !retained.contains(&work.id))
        {
            for suffix in ["nodes", ""] {
                match std::fs::remove_dir(root.join(work_path(&work.id, suffix)?)) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(error) => return Err(fail(error)),
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::super::encoded;
    use super::super::tests::{context as test_context, finish, limits, resources};
    use super::*;
    use lk_core::semantic::SemanticResource;
    use serde_json::json;

    fn context(subject: &str) -> Value {
        let mut context = test_context(subject);
        context["subject"] = json!(Subject::Audit {
            path: format!("wiki/concepts/{subject}.md"),
            file_revision: "captured-file".into(),
            input_revision: "captured-input".into(),
        });
        context
    }

    fn pending_task(root: &Path) -> lk_queue::QueueTask {
        let queue = root.join(".lorekeeper/queue");
        let client = lk_queue::QueueLlmClient::new(queue.clone());
        let task = client.build_task(
            "summary",
            lk_queue::TaskKind::Summarize,
            lk_queue::SummarizeRequest {
                text: "Retained source observation.".into(),
                max_sentences: 3,
                focus: None,
                locale: lk_core::i18n::Locale::En,
                source_type: None,
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/pending.md".into(),
                    kind: lk_queue::TargetKind::DocumentSummary,
                    anchor: "## Summary".into(),
                },
            },
        );
        std::fs::create_dir_all(&queue).unwrap();
        lk_queue::write_tasks_atomic(&queue.join("pending.jsonl"), std::slice::from_ref(&task))
            .unwrap();
        task
    }

    fn publish(work: &Work, session: &MutationSession, resources: &[SemanticResource]) {
        finish(work, session, resources);
        let result = json!({"body":"Published typed result"});
        work.seal(session, resources, result.clone()).unwrap();
        let mut plan = MutationPlan::default();
        work.guard_completion(&mut plan, session.root(), resources, &result)
            .unwrap();
        work.plan_publication(&mut plan, session.root(), &result)
            .unwrap();
        plan.publish_expected(
            session.root(),
            work_path(&work.plan.id, "final.json").unwrap(),
            None,
            encoded(&result).unwrap(),
        )
        .unwrap();
        session.apply(plan).unwrap();
    }

    #[test]
    fn pending_queue_keeps_its_published_work_and_reuse_ancestors_without_blocking_other_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut source = resources();
        source[0].text = "Exact shared evidence.".into();
        let ancestor =
            Work::create(&session, &source, "ancestor", context("ancestor"), limits()).unwrap();
        publish(&ancestor, &session, &source);
        let middle =
            Work::create(&session, &source, "middle", context("middle"), limits()).unwrap();
        middle
            .reuse_from(
                &session,
                &ancestor,
                &ancestor.plan.parts[0].id,
                &middle.plan.parts[0].id,
                &source,
            )
            .unwrap();
        publish(&middle, &session, &source);
        let task = pending_task(root.path());
        let mut current_context = context("current");
        current_context["subject"] = json!(Subject::Queue {
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target: task.target.vault_path.clone(),
            target_revision: "captured-target".into(),
        });
        let current =
            Work::create(&session, &source, "current", current_context, limits()).unwrap();
        current
            .reuse_from(
                &session,
                &middle,
                &middle.plan.parts[0].id,
                &current.plan.parts[0].id,
                &source,
            )
            .unwrap();
        publish(&current, &session, &source);
        let independent = Work::create(
            &session,
            &source,
            "independent",
            context("independent"),
            limits(),
        )
        .unwrap();
        publish(&independent, &session, &source);
        let unpublished = Work::create(
            &session,
            &source,
            "unpublished",
            context("unpublished"),
            limits(),
        )
        .unwrap();
        let queue_path = root.path().join(".lorekeeper/queue/pending.jsonl");
        let queue_lock = root.path().join(".lorekeeper/queue/.lock");
        let queue_before = std::fs::read(&queue_path).unwrap();
        let cutoff = jiff::Timestamp::now().as_second() + 1;
        assert!(!queue_lock.exists());
        let preview = prune_published(root.path(), None, cutoff).unwrap();
        assert_eq!(preview.removed, 1);
        assert_eq!(preview.retained, 4);
        assert!(Work::load(root.path(), &independent.plan.id).is_ok());
        assert_eq!(std::fs::read(&queue_path).unwrap(), queue_before);
        assert!(!queue_lock.exists());
        let actual = prune_published(root.path(), Some(&session), cutoff).unwrap();
        assert_eq!(actual.bytes, preview.bytes);
        assert!(
            !root
                .path()
                .join(work_path(&independent.plan.id, "").unwrap())
                .exists()
        );
        for work in [&ancestor, &middle, &current, &unpublished] {
            assert!(Work::load(root.path(), &work.plan.id).is_ok());
        }
        assert_eq!(std::fs::read(queue_path).unwrap(), queue_before);
    }

    #[test]
    fn unknown_queue_and_work_references_refuse_before_deleting_expired_work() {
        for scenario in [
            "queue-line",
            "queue-input",
            "staged-payload",
            "staged-orphan",
            "work-subject",
            "reuse-reference",
            "mutation-journal",
        ] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let mut source = resources();
            source[0].text = "Exact evidence for retention refusal.".into();
            let work = Work::create(
                &session,
                &source,
                "published",
                context("published"),
                limits(),
            )
            .unwrap();
            publish(&work, &session, &source);
            let task = pending_task(root.path());
            let queue = root.path().join(".lorekeeper/queue");
            match scenario {
                "queue-line" => std::fs::write(queue.join("pending.jsonl"), "not-json\n").unwrap(),
                "queue-input" => {
                    let mut task = task;
                    task.cache_hash = "wrong-input".into();
                    lk_queue::write_tasks_atomic(&queue.join("pending.jsonl"), &[task]).unwrap();
                }
                "staged-payload" | "staged-orphan" => {
                    std::fs::create_dir_all(queue.join("results")).unwrap();
                    let body = if scenario == "staged-payload" {
                        "{}".into()
                    } else {
                        encoded(&lk_queue::SectionResult {
                            task_id: "unknown-task".into(),
                            cache_hash: task.cache_hash,
                            target_revision: "captured".into(),
                            body: "Summary.".into(),
                        })
                        .unwrap()
                    };
                    std::fs::write(queue.join("results/pending.section.json"), body).unwrap();
                }
                "work-subject" => {
                    Work::create(
                        &session,
                        &source,
                        "unknown",
                        test_context("unknown"),
                        limits(),
                    )
                    .unwrap();
                }
                "reuse-reference" => {
                    let path = output_path(&work.plan.id, &work.plan.parts[0].id).unwrap();
                    let mut stored: StoredOutput =
                        serde_json::from_str(&read_optional(root.path(), &path).unwrap().unwrap())
                            .unwrap();
                    stored.reused_from = Some(super::super::Reuse {
                        work_id: "0".repeat(64),
                        node_id: work.plan.parts[0].id.clone(),
                        output_revision: stored.output_revision.clone(),
                        content_revision: "0".repeat(64),
                    });
                    std::fs::write(root.path().join(path), encoded(&stored).unwrap()).unwrap();
                }
                "mutation-journal" => {
                    std::fs::write(
                        root.path().join(".lorekeeper/mutation/pending.json"),
                        "pending",
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            let cutoff = jiff::Timestamp::now().as_second() + 1;
            assert!(
                prune_published(root.path(), None, cutoff).is_err(),
                "{scenario}"
            );
            assert!(
                prune_published(root.path(), Some(&session), cutoff).is_err(),
                "{scenario}"
            );
            assert!(
                root.path()
                    .join(work_path(&work.plan.id, "publication.json").unwrap())
                    .exists(),
                "{scenario}"
            );
        }
    }

    #[test]
    fn cleanup_resumes_after_the_committed_file_removals_without_deleting_unowned_files() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work = Work::create(
            &session,
            &source,
            "published",
            context("published"),
            limits(),
        )
        .unwrap();
        publish(&work, &session, &source);
        let cutoff = jiff::Timestamp::now().as_second() + 1;
        let capture = capture_retention(root.path(), &work.plan.id, cutoff).unwrap();
        let mut plan = MutationPlan::default();
        for (path, digest) in capture.revisions {
            plan.remove(&path, read_captured(root.path(), &path, &digest).unwrap());
        }
        session.apply(plan).unwrap();
        let directory = root.path().join(work_path(&work.plan.id, "").unwrap());
        assert!(directory.exists());
        assert_eq!(
            prune_published(root.path(), None, cutoff)
                .unwrap()
                .empty_directories,
            1
        );
        assert!(directory.exists());
        std::fs::write(directory.join("unowned.txt"), "Preserve this file.").unwrap();
        assert!(prune_published(root.path(), Some(&session), cutoff).is_err());
        assert_eq!(
            std::fs::read_to_string(directory.join("unowned.txt")).unwrap(),
            "Preserve this file."
        );
        std::fs::remove_file(directory.join("unowned.txt")).unwrap();
        assert_eq!(
            prune_published(root.path(), Some(&session), cutoff)
                .unwrap()
                .empty_directories,
            1
        );
        assert!(!directory.exists());
    }

    #[test]
    fn retention_requires_published_receipts_and_preserves_pending_and_sealed_work() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let published = Work::create(
            &session,
            &source,
            "published",
            context("published"),
            limits(),
        )
        .unwrap();
        let sealed =
            Work::create(&session, &source, "sealed", context("sealed"), limits()).unwrap();
        let pending =
            Work::create(&session, &source, "pending", context("pending"), limits()).unwrap();
        finish(&sealed, &session, &source);
        sealed
            .seal(&session, &source, json!({"body":"not published"}))
            .unwrap();
        publish(&published, &session, &source);
        let cutoff = jiff::Timestamp::now().as_second() + 1;
        assert_eq!(prune_published(root.path(), None, 0).unwrap().removed, 0);
        let preview = prune_published(root.path(), None, cutoff).unwrap();
        assert_eq!(preview.removed, 1);
        assert_eq!(preview.retained, 2);
        assert!(preview.bytes > 0);
        assert!(
            root.path()
                .join(work_path(&published.plan.id, "plan.json").unwrap())
                .exists()
        );
        let actual = prune_published(root.path(), Some(&session), cutoff).unwrap();
        assert_eq!(actual.bytes, preview.bytes);
        assert!(
            !root
                .path()
                .join(work_path(&published.plan.id, "").unwrap())
                .exists()
        );
        assert!(Work::load(root.path(), &sealed.plan.id).is_ok());
        assert!(Work::load(root.path(), &pending.plan.id).is_ok());
    }

    #[test]
    fn retention_keeps_reuse_dependencies_and_unowned_files_and_refuses_corruption() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let old = Work::create(&session, &source, "old", context("old"), limits()).unwrap();
        publish(&old, &session, &source);
        let current = Work::create(&session, &source, "new", context("new"), limits()).unwrap();
        current
            .reuse_from(
                &session,
                &old,
                &old.plan.parts[0].id,
                &current.plan.parts[0].id,
                &source,
            )
            .unwrap();
        let independent = Work::create(
            &session,
            &source,
            "independent",
            context("independent"),
            limits(),
        )
        .unwrap();
        publish(&independent, &session, &source);
        let cutoff = jiff::Timestamp::now().as_second() + 1;
        let note = root
            .path()
            .join(work_path(&independent.plan.id, "authored-note.txt").unwrap());
        std::fs::write(&note, "Keep this unowned file.").unwrap();
        assert_eq!(
            prune_published(root.path(), Some(&session), cutoff)
                .unwrap()
                .removed,
            0
        );
        assert!(Work::load(root.path(), &old.plan.id).is_ok());
        std::fs::remove_file(note).unwrap();
        let receipt = root
            .path()
            .join(work_path(&old.plan.id, "publication.json").unwrap());
        let valid = std::fs::read_to_string(&receipt).unwrap();
        std::fs::write(&receipt, "{").unwrap();
        assert!(prune_published(root.path(), Some(&session), cutoff).is_err());
        assert!(Work::load(root.path(), &independent.plan.id).is_ok());
        std::fs::write(receipt, valid).unwrap();
        let report = prune_published(root.path(), Some(&session), cutoff).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.retained, 2);
        assert!(Work::load(root.path(), &old.plan.id).is_ok());
        assert!(Work::load(root.path(), &current.plan.id).is_ok());
    }

    #[test]
    fn publication_receipt_shares_the_canonical_mutations_preflight() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        finish(&work, &session, &source);
        let result = json!({"body":"candidate"});
        work.seal(&session, &source, result.clone()).unwrap();
        let mut plan = MutationPlan::default();
        work.guard_completion(&mut plan, root.path(), &source, &result)
            .unwrap();
        work.plan_publication(&mut plan, root.path(), &result)
            .unwrap();
        plan.require_unchanged(
            "wiki/documents/changed.md",
            "expected prior evidence".into(),
        );
        assert!(session.apply(plan).is_err());
        assert!(
            !root
                .path()
                .join(work_path(&work.plan.id, "publication.json").unwrap())
                .exists()
        );
    }

    #[test]
    fn consumed_source_observations_expire_only_after_canonical_commit_and_queue_release() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let queue = root.path().join(".lorekeeper/queue");
        let task = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
            "extract",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                rss_original: None,
                text: "Retained source observation.".into(),
                source_id: "notes".into(),
                source_type: lk_core::config::SourceType::Manual,
                date: jiff::civil::date(2026, 10, 22),
                focus: None,
                locale: lk_core::i18n::Locale::En,
                categories: vec![],
                target: lk_queue::TaskTarget {
                    vault_path: "wiki/documents/source.md".into(),
                    kind: lk_queue::TargetKind::DocumentConcepts,
                    anchor: "## Related Concepts".into(),
                },
            },
        );
        let context = json!({
            "subject":Subject::Queue {task_id:task.task_id.clone(),cache_hash:task.cache_hash.clone(),
                target:task.target.vault_path.clone(),target_revision:"captured-target".into()},
            "purpose":{"task_kind":"extract-concepts","extraction_scope":{"policy":"source-proposals-v1"}},
        });
        let work = Work::create(&session, &source, "source-phase", context, limits()).unwrap();
        let mut result = lk_queue::TaskResult {
            concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: "captured-target".into(),
            context_revision: "admitted-context".into(),
            discovery: vec![],
            selection: Some(lk_queue::ExtractionSelection {
                policy: lk_queue::ExtractionSelectionPolicy::ProgressiveV1,
                inventory_revision: "complete-inventory".into(),
                decisions_revision: "screened".into(),
            }),
            target: task.target.clone(),
            date: jiff::civil::date(2026, 10, 22),
            concepts: vec![],
        };
        assert!(
            work.plan_consumption(&mut MutationPlan::default(), root.path(), &source, &result)
                .is_err()
        );
        finish(&work, &session, &source);
        result.task_id.push_str("-different");
        assert!(
            work.plan_consumption(&mut MutationPlan::default(), root.path(), &source, &result)
                .is_err()
        );
        result.task_id.clone_from(&task.task_id);
        let receipt = work_path(&work.plan.id, "consumption.json").unwrap();
        let mut blocked = MutationPlan::default();
        work.plan_consumption(&mut blocked, root.path(), &source, &result)
            .unwrap();
        blocked.require_unchanged("wiki/documents/changed.md", "expected".into());
        assert!(session.apply(blocked).is_err());
        assert!(!root.path().join(&receipt).exists());
        let mut canonical = MutationPlan::default();
        work.plan_consumption(&mut canonical, root.path(), &source, &result)
            .unwrap();
        canonical.create(
            &task.target.vault_path,
            "Published exact extraction.".into(),
        );
        session.apply(canonical).unwrap();
        assert!(root.path().join(&receipt).exists());
        for name in ["seal.json", "final.json", "publication.json"] {
            assert!(
                !root
                    .path()
                    .join(work_path(&work.plan.id, name).unwrap())
                    .exists()
            );
        }
        std::fs::create_dir_all(&queue).unwrap();
        lk_queue::write_tasks_atomic(&queue.join("pending.jsonl"), &[task]).unwrap();
        let cutoff = jiff::Timestamp::now().as_second() + 1;
        assert_eq!(
            prune_published(root.path(), None, cutoff).unwrap().removed,
            0
        );
        assert_eq!(
            prune_published(root.path(), Some(&session), cutoff)
                .unwrap()
                .retained,
            1
        );
        std::fs::remove_file(queue.join("pending.jsonl")).unwrap();
        let raw = std::fs::read_to_string(root.path().join(&receipt)).unwrap();
        std::fs::write(root.path().join(&receipt), "{}").unwrap();
        assert!(prune_published(root.path(), Some(&session), cutoff).is_err());
        assert!(Work::load(root.path(), &work.plan.id).is_ok());
        std::fs::write(root.path().join(&receipt), &raw).unwrap();
        assert_eq!(prune_published(root.path(), None, 0).unwrap().removed, 0);
        assert_eq!(
            prune_published(root.path(), None, cutoff).unwrap().removed,
            1
        );
        assert_eq!(
            prune_published(root.path(), Some(&session), cutoff)
                .unwrap()
                .removed,
            1
        );
        assert!(
            !root
                .path()
                .join(work_path(&work.plan.id, "").unwrap())
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("wiki/documents/source.md")).unwrap(),
            "Published exact extraction."
        );
    }
}
