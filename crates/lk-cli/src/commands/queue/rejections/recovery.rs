use super::*;

const DIRECTORY: &str = ".lorekeeper/queue/recovery";
const SCHEMA: &str = "queue-section-recovery-v1";

pub(super) fn has_store(root: &Path) -> miette::Result<bool> {
    let directory = lk_vault::page_path(root, Path::new(DIRECTORY)).into_diagnostic()?;
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(miette::miette!(
            "section recovery store must be a direct directory"
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).into_diagnostic(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    schema: String,
    receipt_revision: String,
    identity: TaskIdentity,
    context_revision: String,
    rejection: String,
    proposal: Proposal,
    proposal_raw: String,
    completions: BTreeMap<PathBuf, String>,
}

impl Recovery {
    fn revision(&self) -> miette::Result<String> {
        digest(&(
            &self.schema,
            &self.identity,
            &self.context_revision,
            &self.rejection,
            &self.proposal,
            &self.proposal_raw,
            &self.completions,
        ))
    }
}

fn path(identity: &TaskIdentity, context: &str) -> miette::Result<PathBuf> {
    Ok(Path::new(DIRECTORY).join(format!(
        "{}.json",
        digest(&(&identity.task_id, &identity.cache_hash, context))?
    )))
}

fn decode(path: &Path, raw: &str) -> miette::Result<Recovery> {
    let value = crate::commands::json::decode_object(raw.as_bytes()).into_diagnostic()?;
    let recovery: Recovery = serde_json::from_value(value).into_diagnostic()?;
    let receipt = super::decode(
        &super::path(&recovery.identity.task_id, &recovery.identity.cache_hash)?,
        &recovery.rejection,
    )?;
    let proposal: lk_queue::SectionResult = serde_json::from_value(
        crate::commands::json::decode_object(recovery.proposal_raw.as_bytes()).into_diagnostic()?,
    )
    .into_diagnostic()?;
    if recovery.schema != SCHEMA
        || recovery.receipt_revision != recovery.revision()?
        || !recovery.identity.is_valid()
        || !is_revision(&recovery.context_revision)
        || path != self::path(&recovery.identity, &recovery.context_revision)?
        || receipt.kind != RejectionKind::SemanticContextChanged
        || receipt.identity != recovery.identity
        || receipt.proposals != [recovery.proposal.clone()]
        || recovery.proposal.revision
            != blake3::hash(recovery.proposal_raw.as_bytes())
                .to_hex()
                .as_str()
        || proposal.task_id != recovery.identity.task_id
        || proposal.cache_hash != recovery.identity.cache_hash
        || proposal
            .context_revision
            .as_ref()
            .is_none_or(|value| !is_revision(value))
        || recovery.completions.keys().any(|path| {
            !path.starts_with(".lorekeeper/work")
                || path.components().count() != 4
                || !matches!(
                    path.file_name().and_then(|value| value.to_str()),
                    Some("final.json" | "seal.json" | "frontier-seal.json")
                )
        })
    {
        return Err(miette::miette!("invalid section recovery receipt"));
    }
    Ok(recovery)
}

pub(super) fn plan_prune(
    root: &Path,
    retained: &BTreeSet<(String, String)>,
    active: &BTreeMap<(String, String), Vec<ProposalSnapshot>>,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    if !has_store(root)? {
        return Ok(0);
    }
    let directory = lk_vault::page_path(root, Path::new(DIRECTORY)).into_diagnostic()?;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).into_diagnostic(),
    };
    let mut removed = 0;
    for entry in entries {
        let path = Path::new(DIRECTORY).join(entry.into_diagnostic()?.file_name());
        let raw = read_optional(root, &path)?
            .ok_or_else(|| miette::miette!("recovery receipt disappeared"))?;
        let receipt = decode(&path, &raw)?;
        let key = (receipt.identity.task_id, receipt.identity.cache_hash);
        if !retained.contains(&key) && !active.contains_key(&key) {
            mutation.remove(path, raw);
            removed += 1;
        } else {
            mutation.require_unchanged(path, raw);
        }
    }
    Ok(removed)
}

#[cfg(unix)]
pub(in crate::commands) fn recover_sections_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<usize> {
    let root = session.root();
    let _queue = super::super::claim_writer(&root.join(".lorekeeper/queue"), false)?;
    let (recovered, plan) = plan_recovery_controlled(root, config, config_revision, read)?;
    if recovered != 0 {
        session.apply(plan).into_diagnostic()?;
    }
    Ok(recovered)
}

#[cfg(unix)]
fn plan_recovery_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<(usize, lk_vault::MutationPlan)> {
    let pending =
        super::super::read_pending_tasks_controlled(&root.join(".lorekeeper/queue"), read)?;
    if pending.unparseable != 0 {
        return Err(miette::miette!(
            "unparseable queue prevents section recovery"
        ));
    }
    let mut task_ids = BTreeSet::new();
    if pending
        .tasks
        .values()
        .any(|task| !task_ids.insert(&task.task_id))
    {
        return Err(miette::miette!(
            "ambiguous pending task id prevents section recovery"
        ));
    }
    let active = active_proposals_controlled(root, read)?;
    let policy = validation_policy();
    let mut plan = lk_vault::MutationPlan::default();
    let mut guards = BTreeMap::new();
    let mut recovered = 0;
    let mut recovered_tasks = BTreeSet::new();
    for (receipt_path, receipt_raw, receipt) in inventory_controlled(root, read)? {
        read.check().into_diagnostic()?;
        if receipt.kind != RejectionKind::SemanticContextChanged
            || receipt.validation_policy != policy
            || receipt.config_revision != config_revision
        {
            continue;
        }
        let key = (
            receipt.identity.task_id.clone(),
            receipt.identity.cache_hash.clone(),
        );
        let Some(task) = pending.tasks.get(&key) else {
            continue;
        };
        let Some(snapshots) = active.get(&key) else {
            continue;
        };
        if snapshots.len() != 1
            || receipt.proposals != [snapshots[0].proposal.clone()]
            || !snapshots[0]
                .proposal
                .path
                .ends_with(lk_queue::SECTION_RESULT_SUFFIX)
            || !task.kind.supports_target(task.target.kind)
            || task.anchors().len() != 1
            || matches!(
                task.kind,
                lk_queue::TaskKind::ExtractConcepts | lk_queue::TaskKind::SynthesizeConcept
            )
            || receipt.task_revision != digest(task)?
            || TaskIdentity::capture(root, task)? != receipt.identity
        {
            continue;
        }
        let classification =
            super::super::classification::capture(root, task, &mut BTreeMap::new())?;
        if classification.status() != super::super::TaskStatus::Current {
            continue;
        }
        let result: lk_queue::SectionResult = serde_json::from_value(
            crate::commands::json::decode_object(snapshots[0].raw.as_bytes()).into_diagnostic()?,
        )
        .into_diagnostic()?;
        if result.context_revision.is_none() {
            continue;
        }
        let target = super::super::resolve_target_path(&task.target.vault_path)
            .ok_or_else(|| miette::miette!("invalid recovery target"))?;
        let original = super::super::read_page(root, &target).into_diagnostic()?;
        if result.target_revision != lk_core::concept::file_revision(&original) {
            continue;
        }
        let content = lk_queue::SynthesisResult {
            task_id: result.task_id.clone(),
            cache_hash: result.cache_hash.clone(),
            target_revision: result.target_revision.clone(),
            context_revision: result.context_revision.clone().unwrap_or_default(),
            synthesis: result.body.clone(),
            related: None,
        };
        if let Err(error) =
            super::super::synthesis::prepare_content(root, &target, &original, task, &content)
        {
            if error.downcast_ref::<PublicationRejection>().is_some() {
                continue;
            }
            return Err(error);
        }
        let (scope, pages) =
            observe_scope_controlled(root, config, config_revision, task, snapshots, read)?;
        if scope != receipt.scope {
            continue;
        }
        let mut transition = lk_vault::MutationPlan::default();
        let Some((context_revision, completions)) = crate::commands::work::plan_section_recovery(
            root,
            config,
            config_revision,
            task,
            &result,
            &mut transition,
            read,
        )?
        else {
            continue;
        };
        let path = self::path(&receipt.identity, &context_revision)?;
        if let Some(raw) = read_optional_controlled(root, &path, read)? {
            decode(&path, &raw)?;
            continue;
        }
        let mut recovery = Recovery {
            schema: SCHEMA.into(),
            receipt_revision: String::new(),
            identity: receipt.identity,
            context_revision,
            rejection: receipt_raw.clone(),
            proposal: snapshots[0].proposal.clone(),
            proposal_raw: snapshots[0].raw.clone(),
            completions,
        };
        recovery.receipt_revision = recovery.revision()?;
        let after = serde_json::to_string(&recovery).into_diagnostic()?;
        decode(&path, &after)?;
        transition
            .publish_expected(root, path, None, after)
            .into_diagnostic()?;
        transition.remove(&snapshots[0].proposal.path, snapshots[0].raw.clone());
        transition.remove(receipt_path, receipt_raw);
        for (path, raw) in pages {
            if guards
                .insert(path, raw.clone())
                .is_some_and(|before| before != raw)
            {
                return Err(miette::miette!(
                    "section recovery scope changed during capture"
                ));
            }
        }
        plan.extend(transition);
        recovered_tasks.insert(key);
        recovered += 1;
    }
    if recovered != 0 {
        let mut guarded = lk_vault::MutationPlan::default();
        let mut found = BTreeSet::new();
        for path in super::super::pending_queue_files(&root.join(".lorekeeper/queue"))? {
            let (raw, file) = lk_queue::read_tasks_snapshot(&path).into_diagnostic()?;
            if !file.is_rewritable() {
                return Err(miette::miette!(
                    "unparseable queue prevents section recovery"
                ));
            }
            let mut referenced = false;
            for task in file.tasks {
                let key = (task.task_id.clone(), task.cache_hash.clone());
                if recovered_tasks.contains(&key) {
                    if !found.insert(key.clone())
                        || serde_json::to_value(&task).into_diagnostic()?
                            != serde_json::to_value(&pending.tasks[&key]).into_diagnostic()?
                    {
                        return Err(miette::miette!(
                            "section recovery task changed during capture"
                        ));
                    }
                    referenced = true;
                }
            }
            if referenced {
                guarded.require_unchanged(
                    path.strip_prefix(root).into_diagnostic()?,
                    String::from_utf8(raw).into_diagnostic()?,
                );
            }
        }
        if found != recovered_tasks {
            return Err(miette::miette!("section recovery task disappeared"));
        }
        for (path, raw) in guards {
            guarded.require_unchanged(path, raw);
        }
        read.check().into_diagnostic()?;
        guarded.extend(plan);
        plan = guarded;
    }
    Ok((recovered, plan))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn section_recovery_preserves_paid_proposals_and_admits_each_context_once() {
        for defect in [
            None,
            Some("body"),
            Some("legacy"),
            Some("conflict"),
            Some("identity"),
            Some("invalid-context"),
            Some("queue-drift"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let config_path = root.join("config.yaml");
            std::fs::write(&config_path, "vault: {root: ., locale: en}\nidentity: {name: Test, email: test@example.com}\nsources:\n  notes: {type: manual, params: {inbox_dir: inbox}}\n").unwrap();
            let (config, revision) =
                lk_core::config::Config::load_with_revision(&config_path).unwrap();
            let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task("summary", lk_queue::TaskKind::Summarize,
                serde_json::from_value::<lk_queue::SummarizeRequest>(json!({"text":"Source.","max_sentences":3,"locale":"en","source_type":"manual",
                    "target":{"vault_path":"wiki/documents/source.md","kind":"document-summary","anchor":"## Summary"}})).unwrap());
            std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
            let target = format!(
                "---\ntype: document\ntitle: Source\nllm_inputs:\n  summary: {}\n---\n## Summary\n\n## Content\nSource.\n",
                task.cache_hash
            );
            std::fs::write(root.join(&task.target.vault_path), &target).unwrap();
            std::fs::create_dir_all(root.join(".lorekeeper/queue/results")).unwrap();
            let tasks = root.join(".lorekeeper/queue/tasks.jsonl");
            lk_queue::write_tasks_atomic(&tasks, std::slice::from_ref(&task)).unwrap();
            let request =
                crate::commands::work::section::prepare(&root, &config, &revision, &task.task_id)
                    .unwrap();
            let crate::commands::work::section::Preparation::Ready(section) = request else {
                panic!("ready section");
            };
            crate::commands::work::section::finish(
                &lk_vault::MutationSession::acquire(&root).unwrap(),
                &config,
                &revision,
                *section,
                crate::commands::execution::finalization::SectionResponse::Complete(
                    "A grounded summary.".into(),
                ),
            )
            .unwrap();
            let active = active_proposals(&root).unwrap();
            let original = &active[&(task.task_id.clone(), task.cache_hash.clone())][0];
            let proposal_path = root.join(&original.proposal.path);
            let mut result: lk_queue::SectionResult = serde_json::from_str(&original.raw).unwrap();
            match defect {
                Some("body") => result.body = "## An injected owned heading".into(),
                Some("legacy") => result.context_revision = None,
                Some("identity") => result.target_revision = "0".repeat(64),
                Some("invalid-context") => result.context_revision = Some("invalid".into()),
                _ => {}
            }
            let raw = serde_json::to_string(&result).unwrap();
            std::fs::write(&proposal_path, &raw).unwrap();
            if defect == Some("invalid-context") {
                assert!(active_proposals(&root).is_err());
                assert_eq!(std::fs::read_to_string(&proposal_path).unwrap(), raw);
                continue;
            }
            if defect == Some("conflict") {
                let mut other = result.clone();
                other.body = "Another answer.".into();
                std::fs::write(
                    root.join(".lorekeeper/queue/results/other.section.json"),
                    serde_json::to_string(&other).unwrap(),
                )
                .unwrap();
            }
            std::fs::create_dir_all(root.join("wiki/explorations")).unwrap();
            std::fs::write(root.join("wiki/explorations/review.md"), "---\ntype: exploration\ntitle: Review\nreview: {status: unresolved, subjects: [wiki/documents/source.md]}\n---\n# Review\nNew issue.\n").unwrap();
            let snapshots = active_proposals(&root).unwrap();
            let snapshots = &snapshots[&(task.task_id.clone(), task.cache_hash.clone())];
            let session = lk_vault::MutationSession::acquire(&root).unwrap();
            let (receipt_path, _) = record(
                &session,
                &config,
                &revision,
                &task,
                snapshots,
                &PublicationRejection {
                    kind: RejectionKind::SemanticContextChanged,
                    detail: "Scoped review changed.".into(),
                },
            )
            .unwrap();
            let tasks_before = std::fs::read(&tasks).unwrap();
            let target_before = std::fs::read(root.join(&task.target.vault_path)).unwrap();
            if defect == Some("queue-drift") {
                let (count, plan) = plan_recovery_controlled(
                    &root,
                    &config,
                    &revision,
                    &lk_vault::ReadControl::default(),
                )
                .unwrap();
                assert_eq!(count, 1);
                drop(session);
                let journal = json!({"version":1,"plan":plan}).to_string();
                let pending = root.join(".lorekeeper/mutation/pending.json");
                std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
                std::fs::write(&pending, &journal).unwrap();
                std::fs::write(&tasks, "").unwrap();
                assert!(lk_vault::MutationSession::acquire(&root).is_err());
                assert_eq!(std::fs::read_to_string(&proposal_path).unwrap(), raw);
                assert!(root.join(receipt_path).exists());
                assert_eq!(std::fs::read_to_string(&pending).unwrap(), journal);
                assert!(!root.join(DIRECTORY).exists());
                continue;
            }
            let recovered = recover_sections_controlled(
                &session,
                &config,
                &revision,
                &lk_vault::ReadControl::default(),
            )
            .unwrap();
            assert_eq!(recovered, usize::from(defect.is_none()), "{defect:?}");
            assert_eq!(std::fs::read(&tasks).unwrap(), tasks_before);
            assert_eq!(
                std::fs::read(root.join(&task.target.vault_path)).unwrap(),
                target_before
            );
            if defect.is_none() {
                assert!(!proposal_path.exists());
                assert!(!root.join(receipt_path).exists());
                let path = std::fs::read_dir(root.join(DIRECTORY))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                let recovery = decode(
                    path.strip_prefix(&root).unwrap(),
                    &std::fs::read_to_string(&path).unwrap(),
                )
                .unwrap();
                assert_eq!(recovery.proposal_raw, raw);
                assert!(recovery.completions.is_empty());
                let crate::commands::work::section::Preparation::Ready(section) =
                    crate::commands::work::section::prepare(
                        &root,
                        &config,
                        &revision,
                        &task.task_id,
                    )
                    .unwrap()
                else {
                    panic!("fresh section");
                };
                crate::commands::work::section::finish(
                    &session,
                    &config,
                    &revision,
                    *section,
                    crate::commands::execution::finalization::SectionResponse::Complete(
                        "A summary addressing the current review.".into(),
                    ),
                )
                .unwrap();
                let applied = crate::commands::queue::synthesis::apply(
                    &root,
                    &root.join(".lorekeeper/queue"),
                    &config,
                    &revision,
                    false,
                    Some(&session),
                )
                .unwrap();
                assert_eq!(applied.applied, 1);
                assert!(
                    std::fs::read_to_string(root.join(&task.target.vault_path))
                        .unwrap()
                        .contains("A summary addressing the current review.")
                );
                std::fs::write(root.join(&task.target.vault_path), &target).unwrap();
                std::fs::write(&proposal_path, &raw).unwrap();
                record(
                    &session,
                    &config,
                    &revision,
                    &task,
                    snapshots,
                    &PublicationRejection {
                        kind: RejectionKind::SemanticContextChanged,
                        detail: "Scoped review changed.".into(),
                    },
                )
                .unwrap();
                assert_eq!(
                    recover_sections_controlled(
                        &session,
                        &config,
                        &revision,
                        &lk_vault::ReadControl::default()
                    )
                    .unwrap(),
                    0
                );
                assert_eq!(std::fs::read_to_string(&proposal_path).unwrap(), raw);
                let mut plan = lk_vault::MutationPlan::default();
                assert_eq!(
                    plan_prune(
                        &root,
                        &BTreeSet::from([(task.task_id.clone(), task.cache_hash.clone())]),
                        &BTreeMap::new(),
                        &mut plan
                    )
                    .unwrap(),
                    0
                );
                let mut plan = lk_vault::MutationPlan::default();
                assert_eq!(
                    plan_prune(&root, &BTreeSet::new(), &BTreeMap::new(), &mut plan).unwrap(),
                    1
                );
                session.apply(plan).unwrap();
                assert!(!path.exists());
            } else {
                assert_eq!(std::fs::read_to_string(&proposal_path).unwrap(), raw);
            }
        }
    }
}
