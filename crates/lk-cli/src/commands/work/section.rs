use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{Prepared, Subject};

pub(in crate::commands) struct Section {
    pub request: Value,
    pub max_response_bytes: usize,
    prepared: Prepared,
    result: lk_queue::SectionResult,
    binding: super::preparation::Binding,
}

pub(in crate::commands) enum Preparation {
    Ready(Box<Section>),
    NeedsAction(String),
    Capacity {
        request_bytes: usize,
        max_input_bytes: usize,
    },
}

impl Section {
    pub fn validate_response(
        &self,
        response: &crate::commands::execution::finalization::SectionResponse,
    ) -> miette::Result<()> {
        if let crate::commands::execution::finalization::SectionResponse::Complete(body) = response
        {
            super::validate_section_response(&self.request, body)?;
        }
        Ok(())
    }
}

use super::action::{Action, SECTION, digest};

fn action_path(task_id: &str, cache_hash: &str) -> PathBuf {
    SECTION.path(task_id, cache_hash)
}
fn read_action(
    root: &Path,
    task_id: &str,
    cache_hash: &str,
) -> miette::Result<Option<(String, Action)>> {
    SECTION.read(root, task_id, cache_hash)
}
pub(in crate::commands) fn plan_prune(
    root: &Path,
    pending: &std::collections::BTreeSet<(String, String)>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    SECTION.plan_prune(root, pending, plan)
}

#[cfg(test)]
pub(in crate::commands) fn prepare(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<Preparation> {
    prepare_controlled(
        &lk_vault::MutationSession::acquire(root).into_diagnostic()?,
        config,
        config_revision,
        task_id,
        &lk_vault::ReadControl::default(),
    )
}

pub(in crate::commands) fn prepare_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<Preparation> {
    let root = session.root();
    let task = crate::commands::queue::current_task_controlled(root, task_id, read)?;
    if !crate::commands::execution::finalization::supports(task.kind) {
        return Err(miette::miette!("task requires interactive processing"));
    }
    let mut original = super::resources::task_controlled(root, config, task_id, read)?;
    let work = super::expected(root, &mut original, config, config_revision)?;
    if work.plan.parts.len() != 1 || work.is_planned(root)? {
        return Err(miette::miette!(
            "existing or partitioned work requires its guarded Work protocol"
        ));
    }
    let prepared = super::resources::section_task_controlled(root, config, task_id, read)?;
    let result = lk_queue::SectionResult {
        task_id: task.task_id.clone(),
        cache_hash: task.cache_hash.clone(),
        target_revision: match &prepared.subject {
            Subject::Queue {
                target_revision, ..
            } => target_revision.clone(),
            _ => unreachable!("queue task"),
        },
        context_revision: Some(prepared.revision(config_revision)?),
        body: String::new(),
    };
    let bounds = super::limits(config)?;
    let envelope = serde_json::to_vec(&result).into_diagnostic()?.len();
    let max_response_bytes = bounds
        .max_result_bytes
        .checked_sub(envelope)
        .and_then(|limit| {
            limit.checked_add(crate::commands::execution::finalization::empty_response_bytes())
        })
        .ok_or_else(|| miette::miette!("section result identity exceeds max_result_bytes"))?;
    let events = (task.kind == lk_queue::TaskKind::RefineEvents)
        .then(|| super::refinement_events(root, &prepared, &task))
        .transpose()?;
    let input = json!({"context":{"subject":prepared.subject,"purpose":prepared.context["purpose"]},
        "context_revision":prepared.revision(config_revision)?,"resources":prepared.resources});
    let request = crate::commands::execution::finalization::whole_request(
        &input,
        events,
        max_response_bytes,
    )?;
    let request_bytes = serde_json::to_vec(&request).into_diagnostic()?.len();
    if request_bytes > bounds.max_input_bytes {
        return Ok(Preparation::Capacity {
            request_bytes,
            max_input_bytes: bounds.max_input_bytes,
        });
    }
    if let Some((_, action)) = read_action(root, task_id, &task.cache_hash)?
        && action.request_revision == digest(&request)?
    {
        if action.context_revision != prepared.revision(config_revision)? {
            return Err(miette::miette!(
                "section action context differs from its request"
            ));
        }
        return Ok(Preparation::NeedsAction(action.reason));
    }
    let binding = super::preparation::Binding::build(
        super::preparation::Key::WholeSection {
            task_id: task.task_id,
            cache_hash: task.cache_hash,
        },
        task.target.vault_path,
        result.target_revision.clone(),
        &json!({"work":request["work"],"events":request["events"]}),
        &request,
        &crate::commands::execution::finalization::schema(),
    )?;
    if let Some(reason) = binding.held(root)? {
        return Ok(Preparation::NeedsAction(reason));
    }
    Ok(Preparation::Ready(Box::new(Section {
        request,
        max_response_bytes,
        prepared,
        result,
        binding,
    })))
}

pub(in crate::commands) fn hold(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    section: Section,
    failure: super::state::InvocationFailure,
) -> miette::Result<Option<String>> {
    let read = section.prepared.read_control().clone();
    let current = match prepare_controlled(
        session,
        config,
        config_revision,
        &section.result.task_id,
        &read,
    ) {
        Ok(Preparation::Ready(current)) if current.request == section.request => current,
        Ok(_) => return Ok(None),
        Err(error) if super::is_work_input_changed(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut plan = current.binding.plan_record(
        session.root(),
        super::preparation::Disposition::invocation_failed(failure),
    )?;
    current.prepared.guard_pages(&mut plan);
    let _queue =
        crate::commands::queue::claim_writer(&session.root().join(".lorekeeper/queue"), false)?;
    let task = match crate::commands::queue::current_task_controlled(
        session.root(),
        &section.result.task_id,
        &read,
    ) {
        Ok(task) => task,
        Err(error) if super::is_work_input_changed(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    crate::commands::queue::guard_current_task(session.root(), &task, &mut plan)?;
    read.check().into_diagnostic()?;
    session.apply(plan).map_err(crate::commands::typed_error)?;
    current
        .binding
        .held(session.root())?
        .map(Some)
        .ok_or_else(|| miette::miette!("section hold changed after recording"))
}

pub(in crate::commands) fn finish(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    section: Section,
    response: crate::commands::execution::finalization::SectionResponse,
) -> miette::Result<Option<String>> {
    let root = session.root();
    let read = section.prepared.read_control().clone();
    let current = prepare_controlled(
        session,
        config,
        config_revision,
        &section.result.task_id,
        &read,
    )?;
    let Preparation::Ready(current) = current else {
        return Err(miette::miette!(
            "section requires a fresh action before publication"
        ));
    };
    if current.request != section.request {
        return Err(miette::Report::new(super::SourceChanged::new(
            "section inputs changed; retained work must be reread",
        )));
    }
    let path = action_path(&section.result.task_id, &section.result.cache_hash);
    let before =
        read_action(root, &section.result.task_id, &section.result.cache_hash)?.map(|(raw, _)| raw);
    let mut plan = lk_vault::MutationPlan::default();
    current.prepared.guard_pages(&mut plan);
    match response {
        crate::commands::execution::finalization::SectionResponse::NeedsAction(reason)
        | crate::commands::execution::finalization::SectionResponse::InvalidBody(reason) => {
            let action = Action {
                schema: SECTION.schema.into(),
                task_id: section.result.task_id,
                cache_hash: section.result.cache_hash,
                context_revision: current.prepared.revision(config_revision)?,
                request_revision: digest(&section.request)?,
                reason: reason.clone(),
                exhausted: false,
            };
            plan.publish_expected(
                root,
                path,
                before,
                serde_json::to_string(&action).into_diagnostic()?,
            )
            .into_diagnostic()?;
            read.check().into_diagnostic()?;
            session.apply(plan).map_err(crate::commands::typed_error)?;
            Ok(Some(reason))
        }
        crate::commands::execution::finalization::SectionResponse::Complete(body) => {
            super::validate_section_response(&section.request, &body)?;
            let result = lk_queue::SectionResult {
                body,
                ..section.result
            };
            let raw = serde_json::to_string(&result).into_diagnostic()?;
            if raw.len() > super::limits(config)?.max_result_bytes {
                return Err(miette::miette!(
                    "complete section result exceeds max_result_bytes"
                ));
            }
            let value = serde_json::to_value(&result).into_diagnostic()?;
            super::guard_queue_controlled(
                root,
                config,
                config_revision,
                &result.task_id,
                value.clone(),
                None,
                &mut plan,
                &read,
            )?;
            let destination = super::queue_result_path(
                crate::commands::queue::current_task(root, &result.task_id)?.kind,
                &value,
            )?;
            plan.publish_expected(root, destination, None, raw)
                .into_diagnostic()?;
            if let Some(before) = before {
                plan.remove(path, before);
            }
            read.check().into_diagnostic()?;
            session.apply(plan).map_err(crate::commands::typed_error)?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn staged_sections_reject_new_scoped_reviews_without_retagging() {
        for bounded in [false, true] {
            assert_staged_review_scope(bounded, false);
        }
        assert_staged_review_scope(true, true);
    }

    #[cfg(unix)]
    fn assert_staged_review_scope(bounded: bool, published: bool) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(&config_path,
            "vault: {root: ., locale: en}\nidentity: {name: Test, email: test@example.com}\nsources:\n  notes: {type: manual, params: {inbox_dir: inbox}}\n").unwrap();
        let (config, revision) = lk_core::config::Config::load_with_revision(&config_path).unwrap();
        let queue = root.join(".lorekeeper/queue");
        std::fs::create_dir_all(&queue).unwrap();
        std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
        let target = "wiki/documents/source.md";
        let task = lk_queue::QueueLlmClient::new(queue.clone()).build_task(
            "fixture",
            lk_queue::TaskKind::Summarize,
            serde_json::from_value::<lk_queue::SummarizeRequest>(json!({"text":"A proposed decision.","max_sentences":3,
                "locale":"en","source_type":"manual","target":{"vault_path":target,"anchor":"## Summary","kind":"document-summary"}})).unwrap(),
        );
        std::fs::write(root.join(target), format!("---\ntype: document\ntitle: Source\nllm_inputs:\n  summary: {}\n---\n# Source\n\n## Summary\n\n## Content\nA proposed decision.\n",task.cache_hash)).unwrap();
        lk_queue::write_tasks_atomic(&queue.join("tasks.jsonl"), std::slice::from_ref(&task))
            .unwrap();
        let body = "A proposal, not a verified deployment.".to_string();
        let result = if bounded {
            let mut prepared =
                super::super::resources::task(&root, &config, &task.task_id).unwrap();
            let id = {
                let session = lk_vault::MutationSession::acquire(&root).unwrap();
                let work =
                    super::super::persist_prepared(&session, &config, &revision, &mut prepared)
                        .unwrap();
                loop {
                    let (progress, request) =
                        work.prepare_inspection(&root, &prepared.resources).unwrap();
                    if progress.complete {
                        break;
                    }
                    let request = request.unwrap();
                    work.record(
                        &session,
                        super::super::WorkOutput {
                            work_id: work.plan.id.clone(),
                            node_id: request["node_id"].as_str().unwrap().into(),
                            input_revision: request["input_revision"].as_str().unwrap().into(),
                            summary: "Exact saved source read.".into(),
                            findings: vec![],
                            source_questions: vec![],
                            unresolved: vec![],
                            resolutions: vec![],
                        },
                        &prepared.resources,
                    )
                    .unwrap();
                }
                work.plan.id
            };
            let super::super::FinalizationPreparation::Ready(section) =
                super::super::prepare_finalization(&root, &config, &revision, &id).unwrap()
            else {
                panic!("ready bounded section");
            };
            let result = lk_queue::SectionResult {
                body: body.clone(),
                ..section.result.clone()
            };
            super::super::finish_section(
                &lk_vault::MutationSession::acquire(&root).unwrap(),
                &config,
                &revision,
                &id,
                *section,
                body,
                &lk_vault::ReadControl::default(),
            )
            .unwrap();
            result
        } else {
            let Preparation::Ready(section) =
                prepare(&root, &config, &revision, &task.task_id).unwrap()
            else {
                panic!("ready section");
            };
            let result = lk_queue::SectionResult {
                body: body.clone(),
                ..section.result.clone()
            };
            finish(
                &lk_vault::MutationSession::acquire(&root).unwrap(),
                &config,
                &revision,
                *section,
                crate::commands::execution::finalization::SectionResponse::Complete(body),
            )
            .unwrap();
            result
        };
        let value = serde_json::to_value(&result).unwrap();
        super::super::prepare_queue_publication(
            &root,
            &config,
            &revision,
            &task.task_id,
            value.clone(),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("wiki/explorations")).unwrap();
        std::fs::write(root.join("wiki/explorations/review.md"),
            format!("---\ntype: exploration\ntitle: Review\nreview:\n  status: unresolved\n  subjects: [{target}]\n---\n# Review\nNew scoped issue.\n")).unwrap();
        let error = super::super::prepare_queue_publication(
            &root,
            &config,
            &revision,
            &task.task_id,
            value,
        )
        .err()
        .expect("changed review scope refuses");
        assert!(
            error
                .to_string()
                .contains("section review/read scope changed"),
            "{error}"
        );
        let path =
            super::super::queue_result_path(task.kind, &serde_json::to_value(&result).unwrap())
                .unwrap();
        let retained: lk_queue::SectionResult =
            serde_json::from_slice(&std::fs::read(root.join(&path)).unwrap()).unwrap();
        assert_eq!(retained.context_revision, result.context_revision);
        if bounded {
            let mut prepared =
                super::super::resources::task(&root, &config, &task.task_id).unwrap();
            let work = super::super::expected(&root, &mut prepared, &config, &revision).unwrap();
            let nodes = root
                .join(".lorekeeper/work")
                .join(&work.plan.id)
                .join("nodes");
            let paid = std::fs::read_dir(&nodes)
                .unwrap()
                .map(|entry| {
                    let path = entry.unwrap().path();
                    (path.clone(), std::fs::read(path).unwrap())
                })
                .collect::<Vec<_>>();
            assert!(!super::super::is_finished(&root, &config, &revision, &work.plan.id).unwrap());
            let session = lk_vault::MutationSession::acquire(&root).unwrap();
            assert!(
                super::super::restage_finished(
                    &session,
                    &config,
                    &revision,
                    &work.plan.id,
                    &lk_vault::ReadControl::default()
                )
                .is_err()
            );
            if published {
                let mut publication = lk_vault::MutationPlan::default();
                work.plan_publication(
                    &mut publication,
                    &root,
                    &serde_json::to_value(&result).unwrap(),
                )
                .unwrap();
                session.apply(publication).unwrap();
                assert!(
                    super::super::is_finished(&root, &config, &revision, &work.plan.id).is_err()
                );
            } else {
                let mut guards = lk_vault::MutationPlan::default();
                let (_, completions) = super::super::plan_section_recovery(
                    &root,
                    &config,
                    &revision,
                    &task,
                    &result,
                    &mut guards,
                    &lk_vault::ReadControl::default(),
                )
                .unwrap()
                .unwrap();
                assert_eq!(completions.len(), 2);
                let final_path = Path::new(".lorekeeper/work")
                    .join(&work.plan.id)
                    .join("final.json");
                assert_eq!(
                    serde_json::from_str::<Value>(&completions[&final_path]).unwrap(),
                    serde_json::to_value(&result).unwrap()
                );
                session.apply(guards).unwrap();
                std::fs::remove_file(root.join(&path)).unwrap();
                let super::super::FinalizationPreparation::Ready(fresh) =
                    super::super::prepare_finalization(&root, &config, &revision, &work.plan.id)
                        .unwrap()
                else {
                    panic!("fresh section");
                };
                assert_ne!(fresh.result.context_revision, result.context_revision);
                super::super::finish_section(
                    &session,
                    &config,
                    &revision,
                    &work.plan.id,
                    *fresh,
                    "A summary of the source with the current review.".into(),
                    &lk_vault::ReadControl::default(),
                )
                .unwrap();
                assert!(
                    super::super::is_finished(&root, &config, &revision, &work.plan.id).unwrap()
                );
            }
            for (path, raw) in paid {
                assert_eq!(std::fs::read(path).unwrap(), raw);
            }
        }
    }
}
