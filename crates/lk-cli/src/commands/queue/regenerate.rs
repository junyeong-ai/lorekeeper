use std::path::{Path, PathBuf};

use super::{
    Artifact, TaskStatus, claim_writer, classify_against_page, read_source_revision,
    resolve_target_path, resolve_vault_root,
};

pub(super) fn run(
    opts: &super::super::GlobalOptions,
    root: Option<PathBuf>,
    task_id: String,
    dry_run: bool,
) -> miette::Result<()> {
    let root = resolve_vault_root(opts, root)?;
    let session = (!dry_run)
        .then(|| lk_vault::MutationSession::acquire(&root))
        .transpose()
        .map_err(|e| miette::miette!("{e}"))?;
    if dry_run
        && root
            .join(".lorekeeper/mutation/pending.json")
            .try_exists()
            .map_err(|e| miette::miette!("{e}"))?
    {
        return Err(miette::miette!(
            "pending mutation must be recovered before regeneration preview"
        ));
    }
    let queue = root.join(".lorekeeper/queue");
    let _claim = claim_writer(&queue, dry_run)?;
    let mut matches = Vec::new();
    let mut pending = Vec::new();
    for dir in [queue.clone(), queue.join(lk_queue::PROCESSED_SUBDIR)] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(miette::miette!("read {}: {e}", dir.display())),
        };
        for entry in entries {
            let path = entry.map_err(|e| miette::miette!("{e}"))?.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let (raw, read) = lk_queue::read_tasks_snapshot(&path)
                .map_err(|error| miette::miette!("read {}: {error}", path.display()))?;
            if read.unparseable > 0 {
                return Err(miette::miette!(
                    "retained queue has unparseable lines: {}; repair before selecting a unique task",
                    path.display()
                ));
            }
            let raw = String::from_utf8(raw).map_err(|error| miette::miette!("{error}"))?;
            if path.parent() == Some(queue.as_path()) {
                pending.extend(read.tasks.iter().cloned());
            }
            matches.extend(
                read.tasks
                    .into_iter()
                    .enumerate()
                    .filter(|(_, task)| task.task_id == task_id)
                    .map(|(index, task)| (path.clone(), raw.clone(), index, task)),
            );
        }
    }
    if matches.len() != 1 {
        return Err(miette::miette!(
            "expected exactly one retained task {task_id}; found {}. Missing original input cannot be recovered by remote refetch",
            matches.len()
        ));
    }
    let (retained_path, retained_raw, retained_index, mut task) =
        matches.pop().expect("one retained task");
    task.validate_retained_input()
        .map_err(|e| miette::miette!("{e}"))?;
    // The retained policy is historical provenance, not the requested output policy.
    // Regeneration selects this build's current policy after validating the unchanged
    // typed data input, so a future policy bump cannot strand otherwise reusable inputs.
    let target = resolve_target_path(&task.target.vault_path)
        .ok_or_else(|| miette::miette!("invalid retained target"))?;
    let before = super::read_page(&root, &target).map_err(|e| miette::miette!("{e}"))?;
    let state = classify_against_page(
        &root,
        &target,
        task.target.kind,
        &task.cache_hash,
        &task.anchors(),
        Artifact::Result,
    )?;
    if state != TaskStatus::Current {
        return Err(miette::miette!(
            "retained input no longer matches current target/anchors: {}",
            state.as_str()
        ));
    }
    if task.kind == lk_queue::TaskKind::SynthesizeConcept {
        let citations: Vec<lk_core::concept::Citation> = serde_json::from_value(
            task.input
                .get("citations")
                .cloned()
                .ok_or_else(|| miette::miette!("missing citations"))?,
        )
        .map_err(|e| miette::miette!("{e}"))?;
        for citation in citations {
            if citation.rss_event.is_some() {
                lk_vault::validate_rss_citation(&root, &target, &citation)
                    .map_err(|e| miette::miette!("{e}"))?;
                continue;
            }
            let path = resolve_target_path(&citation.path)
                .ok_or_else(|| miette::miette!("invalid citation"))?;
            if read_source_revision(&root, &path).ok().as_deref() != Some(&citation.revision) {
                return Err(miette::miette!(
                    "source revision changed or unreadable: {}; reconcile evidence before regeneration",
                    citation.path
                ));
            }
        }
    }
    let policy = task.expected_generation_policy().to_owned();
    let mut request_input = task.input.clone();
    request_input.insert("generation_policy".into(), policy.clone().into());
    request_input.insert("regenerated_from".into(), task_id.clone().into());
    let mut generation_input = request_input.clone();
    generation_input.remove("regenerated_from");
    for candidate in pending {
        let mut candidate_input = candidate.input.clone();
        candidate_input.remove("regenerated_from");
        if candidate.task_id != task.task_id
            && candidate.kind == task.kind
            && candidate.cache_hash == task.cache_hash
            && candidate.target == task.target
            && candidate_input == generation_input
        {
            match super::classify_task(&root, &candidate, &mut Default::default())? {
                TaskStatus::Current => {
                    return Err(miette::miette!(
                        "regeneration already pending for retained task {task_id}: {}; finish that request before requesting another",
                        candidate.task_id
                    ));
                }
                TaskStatus::Done => {
                    return Err(miette::miette!(
                        "matching completed request remains pending: {}; run `lore queue prune` before regenerating again",
                        candidate.task_id
                    ));
                }
                TaskStatus::Stale | TaskStatus::MissingTarget | TaskStatus::Unreadable => {}
            }
        }
    }
    let mut after = lk_vault::set_llm_input(&before, &task.target.kind.completion_key(), "null")
        .ok_or_else(|| miette::miette!("cannot invalidate completion marker"))?;
    after = lk_vault::set_llm_input(
        &after,
        &format!("{}_policy_done", task.target.kind.llm_inputs_key()),
        "null",
    )
    .ok_or_else(|| miette::miette!("cannot invalidate policy provenance"))?;
    for (suffix, value) in [
        ("policy_requested", policy.as_str()),
        ("policy_requested_input", task.cache_hash.as_str()),
    ] {
        after = lk_vault::set_llm_input(
            &after,
            &format!("{}_{}", task.target.kind.llm_inputs_key(), suffix),
            &serde_json::to_string(value).expect("request serializes"),
        )
        .ok_or_else(|| miette::miette!("cannot record explicit policy request"))?;
    }
    let run = lk_queue::QueueLlmClient::new(queue.clone());
    task.task_id = format!("regen-{}-000", run.run_id());
    task.created_at = jiff::Timestamp::now();
    task.input = request_input;
    let queue_path =
        PathBuf::from(".lorekeeper/queue").join(format!("{}.regenerate.jsonl", run.run_id()));
    let raw = serde_json::to_string(&task).map_err(|e| miette::miette!("{e}"))? + "\n";
    let mut plan = lk_vault::MutationPlan::default();
    if retained_path.parent() == Some(queue.as_path()) {
        let archive_path = PathBuf::from(".lorekeeper/queue")
            .join(lk_queue::PROCESSED_SUBDIR)
            .join(format!("{}.regenerated.jsonl", run.run_id()));
        plan_pending_retirement(
            &mut plan,
            &root,
            &retained_path,
            retained_raw,
            retained_index,
            archive_path,
        )?;
    } else {
        let relative = retained_path
            .strip_prefix(&root)
            .map_err(|error| miette::miette!("{error}"))?;
        plan.require_unchanged(relative, retained_raw);
    }
    // Invalidate before publishing: the new request must never be visible while
    // its old completion marker still says done, including during recovery.
    plan_target_invalidation(&mut plan, &target, before, after);
    plan.create(queue_path, raw);
    if dry_run {
        plan.validate_addresses(&root)
            .map_err(|e| miette::miette!("{e}"))?;
    } else {
        session
            .as_ref()
            .expect("write owns mutation session")
            .apply(plan)
            .map_err(|e| miette::miette!("{e}"))?;
    }
    println!(
        "{}",
        serde_json::json!({"ok":true,"data":{"task_id":task.task_id,
        "regenerated_from":task_id,"generation_policy":policy,"dry_run":dry_run}})
    );
    Ok(())
}

fn plan_target_invalidation(
    plan: &mut lk_vault::MutationPlan,
    target: &Path,
    before: String,
    after: String,
) {
    if before == after {
        plan.require_unchanged(target, before);
    } else {
        plan.edit(target, before, after);
    }
}

fn plan_pending_retirement(
    plan: &mut lk_vault::MutationPlan,
    root: &Path,
    retained_path: &Path,
    retained_raw: String,
    retained_index: usize,
    archive_path: PathBuf,
) -> miette::Result<()> {
    let mut remaining = String::new();
    let mut archived = String::new();
    let mut index = 0;
    for line in retained_raw.split_inclusive('\n') {
        if line.bytes().all(|byte| byte.is_ascii_whitespace()) {
            remaining.push_str(line);
            continue;
        }
        if index == retained_index {
            archived.push_str(line);
        } else {
            remaining.push_str(line);
        }
        index += 1;
    }
    // Archive exact historical bytes before withdrawing the superseded request.
    // Keep them guarded after retirement and throughout interrupted publication.
    plan.create(&archive_path, archived.clone());
    plan.require_unchanged(archive_path, archived);
    let relative = retained_path
        .strip_prefix(root)
        .map_err(|error| miette::miette!("{error}"))?;
    if remaining.bytes().all(|byte| byte.is_ascii_whitespace()) {
        plan.remove(relative, retained_raw);
    } else {
        plan.edit(relative, retained_raw, remaining);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_invalidation_still_guards_target_before_publication_and_recovery() {
        for completed in [0, 1] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let mut plan = lk_vault::MutationPlan::default();
            plan_target_invalidation(
                &mut plan,
                Path::new("page.md"),
                "captured".into(),
                "captured".into(),
            );
            plan.create("replacement.jsonl", "new request".into());
            let plan_revision =
                lk_core::concept::file_revision(&serde_json::to_string(&plan).unwrap());
            let journal = serde_json::json!({"version":1,"plan":plan}).to_string();
            let mutation = root.join(".lorekeeper/mutation");
            std::fs::create_dir_all(&mutation).unwrap();
            std::fs::write(mutation.join("pending.json"), &journal).unwrap();
            std::fs::write(mutation.join("progress.json"), serde_json::json!({
                "plan_revision":plan_revision,"completed":completed,
                "checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{completed}")),
            }).to_string()).unwrap();
            std::fs::write(root.join("page.md"), "external edit").unwrap();
            assert!(lk_vault::MutationSession::acquire(root).is_err());
            assert!(!root.join("replacement.jsonl").exists());
            assert_eq!(
                std::fs::read_to_string(mutation.join("pending.json")).unwrap(),
                journal
            );
            std::fs::write(root.join("page.md"), "captured").unwrap();
            let _recovered = lk_vault::MutationSession::acquire(root).unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("replacement.jsonl")).unwrap(),
                "new request"
            );
            assert!(!mutation.join("pending.json").exists());
        }
    }

    #[test]
    fn interrupted_regeneration_retains_archive_guards_after_pending_retirement() {
        for missing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let queue = root.join(".lorekeeper/queue");
            std::fs::create_dir_all(queue.join("processed")).unwrap();
            let pending = queue.join("retained.jsonl");
            let original = serde_json::json!({"task_id":"retained", "kind":"summarize", "created_at":"2026-10-16T00:00:00Z", "cache_hash":"captured", "input":{"text":"Retained source.","max_sentences":3,"locale":"en"}, "target":{"vault_path":"page.md","kind":"document-summary","anchor":"## Summary"}}).to_string();
            std::fs::write(&pending, &original).unwrap();
            std::fs::write(root.join("page.md"), "before").unwrap();
            let archive = PathBuf::from(".lorekeeper/queue/processed/retained.jsonl");
            let mut plan = lk_vault::MutationPlan::default();
            plan_pending_retirement(
                &mut plan,
                root,
                &pending,
                original.clone(),
                0,
                archive.clone(),
            )
            .unwrap();
            plan.edit("page.md", "before".into(), "completion invalidated".into());
            plan.create(".lorekeeper/queue/new.jsonl", "new request".into());
            let plan_revision =
                lk_core::concept::file_revision(&serde_json::to_string(&plan).unwrap());
            let journal = serde_json::json!({"version":1,"plan":plan}).to_string();
            let mutation = root.join(".lorekeeper/mutation");
            std::fs::create_dir_all(&mutation).unwrap();
            std::fs::write(mutation.join("pending.json"), &journal).unwrap();
            let completed = 3;
            std::fs::write(mutation.join("progress.json"), serde_json::json!({
                "plan_revision":plan_revision,
                "completed":completed,
                "checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{completed}")),
            }).to_string()).unwrap();
            std::fs::remove_file(&pending).unwrap();
            if !missing {
                std::fs::write(root.join(&archive), "changed archived input").unwrap();
            }
            assert!(lk_vault::MutationSession::acquire(root).is_err());
            assert_eq!(
                std::fs::read_to_string(mutation.join("pending.json")).unwrap(),
                journal
            );
            assert_eq!(
                std::fs::read_to_string(root.join("page.md")).unwrap(),
                "before"
            );
            assert!(!queue.join("new.jsonl").exists());
            std::fs::write(root.join(archive), &original).unwrap();
            let _recovered = lk_vault::MutationSession::acquire(root).unwrap();
            assert!(!pending.exists());
            assert_eq!(
                std::fs::read_to_string(root.join("page.md")).unwrap(),
                "completion invalidated"
            );
            assert_eq!(
                std::fs::read_to_string(queue.join("new.jsonl")).unwrap(),
                "new request"
            );
            assert!(!mutation.join("pending.json").exists());
        }
    }
}
