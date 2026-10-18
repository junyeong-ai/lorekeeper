use std::path::PathBuf;

use super::{
    Artifact, TaskStatus, claim_writer, classify_against_page, read_source_revision, read_tasks,
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
            let read = read_tasks(&path)?;
            if read.unparseable > 0 {
                return Err(miette::miette!(
                    "retained queue has unparseable lines: {}; repair before selecting a unique task",
                    path.display()
                ));
            }
            matches.extend(read.tasks.into_iter().filter(|t| t.task_id == task_id));
        }
    }
    if matches.len() != 1 {
        return Err(miette::miette!(
            "expected exactly one retained task {task_id}; found {}. Missing original input cannot be recovered by remote refetch",
            matches.len()
        ));
    }
    let mut task = matches.pop().expect("one retained task");
    task.validate_retained_input()
        .map_err(|e| miette::miette!("{e}"))?;
    // The retained policy is historical provenance, not the requested output policy.
    // Regeneration selects this build's current policy after validating the unchanged
    // typed data input, so a future policy bump cannot strand otherwise reusable inputs.
    let target = resolve_target_path(&task.target.vault_path)
        .ok_or_else(|| miette::miette!("invalid retained target"))?;
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
    let before = super::read_page(&root, &target).map_err(|e| miette::miette!("{e}"))?;
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
    task.input
        .insert("generation_policy".into(), policy.clone().into());
    task.input
        .insert("regenerated_from".into(), task_id.clone().into());
    let queue_path =
        PathBuf::from(".lorekeeper/queue").join(format!("{}.regenerate.jsonl", run.run_id()));
    let raw = serde_json::to_string(&task).map_err(|e| miette::miette!("{e}"))? + "\n";
    if !dry_run {
        let mut plan = lk_vault::MutationPlan::default();
        // Invalidate first: recovery may precede a queue-only pruner's claim. The new
        // task must never be visible while its old completion marker still says done.
        plan.edit(target, before, after);
        plan.create(queue_path, raw);
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
