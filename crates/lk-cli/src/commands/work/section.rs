use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Prepared, Subject};

const SCHEMA: &str = "whole-section-action-v1";

pub(in crate::commands) struct Section {
    pub request: Value,
    pub max_response_bytes: usize,
    prepared: Prepared,
    result: lk_queue::SectionResult,
}

pub(in crate::commands) enum Preparation {
    Ready(Box<Section>),
    NeedsAction(String),
    Capacity {
        request_bytes: usize,
        max_input_bytes: usize,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Action {
    schema: String,
    task_id: String,
    cache_hash: String,
    context_revision: String,
    request_revision: String,
    reason: String,
}

fn action_path(task_id: &str, cache_hash: &str) -> PathBuf {
    let identity = serde_json::to_vec(&(task_id, cache_hash)).expect("identity serializes");
    PathBuf::from(".lorekeeper/queue/section-actions")
        .join(format!("{}.json", blake3::hash(&identity).to_hex()))
}

fn digest(value: &Value) -> miette::Result<String> {
    Ok(lk_core::concept::file_revision(
        &serde_json::to_string(value).into_diagnostic()?,
    ))
}

fn read_action(
    root: &Path,
    task_id: &str,
    cache_hash: &str,
) -> miette::Result<Option<(String, Action)>> {
    let path = action_path(task_id, cache_hash);
    let absolute = lk_vault::page_path(root, &path).into_diagnostic()?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "section action must be a direct regular file"
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    }
    let raw = std::fs::read_to_string(absolute).into_diagnostic()?;
    let action = decode(&path, &raw)?;
    if action.task_id != task_id || action.cache_hash != cache_hash {
        return Err(miette::miette!("section action belongs to another task"));
    }
    Ok(Some((raw, action)))
}

fn decode(path: &Path, raw: &str) -> miette::Result<Action> {
    let value: Value = serde_json::from_str(raw).into_diagnostic()?;
    if !value.is_object() {
        return Err(miette::miette!("section action must be an object"));
    }
    let action: Action = serde_json::from_value(value).into_diagnostic()?;
    let revision = |value: &str, length: usize| {
        value.len() == length
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    if action.schema != SCHEMA
        || action.task_id.trim().is_empty()
        || !revision(&action.cache_hash, 32)
        || path != action_path(&action.task_id, &action.cache_hash)
        || !revision(&action.context_revision, 64)
        || !revision(&action.request_revision, 64)
        || action.reason.trim().is_empty()
    {
        return Err(miette::miette!("invalid section action identity or scope"));
    }
    Ok(action)
}

pub(in crate::commands) fn prune(
    session: &lk_vault::MutationSession,
    pending: &std::collections::BTreeSet<(String, String)>,
) -> miette::Result<usize> {
    let directory = Path::new(".lorekeeper/queue/section-actions");
    let absolute = lk_vault::page_path(session.root(), directory).into_diagnostic()?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "section action store must be a direct directory"
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).into_diagnostic(),
    }
    let mut plan = lk_vault::MutationPlan::default();
    let mut removed = 0;
    for entry in std::fs::read_dir(absolute).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        if !entry.file_type().into_diagnostic()?.is_file() {
            return Err(miette::miette!("unknown section action artifact"));
        }
        let path = directory.join(entry.file_name());
        let raw = std::fs::read_to_string(entry.path()).into_diagnostic()?;
        let action = decode(&path, &raw)?;
        if !pending.contains(&(action.task_id, action.cache_hash)) {
            plan.remove(path, raw);
            removed += 1;
        }
    }
    if removed != 0 {
        session.apply(plan).into_diagnostic()?;
    }
    Ok(removed)
}

pub(in crate::commands) fn prepare(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<Preparation> {
    let root = root.canonicalize().into_diagnostic()?;
    let task = crate::commands::queue::current_task(&root, task_id)?;
    if !crate::commands::execution::finalization::supports(task.kind) {
        return Err(miette::miette!("task requires interactive processing"));
    }
    let original = super::resources::task(&root, config, task_id)?;
    let work = super::expected(&root, &original, config, config_revision)?;
    if work.plan.parts.len() != 1 || work.is_planned(&root)? {
        return Err(miette::miette!(
            "existing or partitioned work requires its guarded Work protocol"
        ));
    }
    let prepared = super::resources::section_task(&root, config, task_id)?;
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
        .then(|| super::refinement_events(&root, &prepared, &task))
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
    if let Some((_, action)) = read_action(&root, task_id, &task.cache_hash)?
        && action.request_revision == digest(&request)?
    {
        if action.context_revision != prepared.revision(config_revision)? {
            return Err(miette::miette!(
                "section action context differs from its request"
            ));
        }
        return Ok(Preparation::NeedsAction(action.reason));
    }
    Ok(Preparation::Ready(Box::new(Section {
        request,
        max_response_bytes,
        prepared,
        result,
    })))
}

pub(in crate::commands) fn finish(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    section: Section,
    response: crate::commands::execution::finalization::SectionResponse,
) -> miette::Result<Option<String>> {
    let root = session.root();
    let current = prepare(root, config, config_revision, &section.result.task_id)?;
    let Preparation::Ready(current) = current else {
        return Err(miette::miette!(
            "section requires a fresh action before publication"
        ));
    };
    if current.request != section.request {
        return Err(miette::miette!(
            "section inputs changed; retained work must be reread"
        ));
    }
    let path = action_path(&section.result.task_id, &section.result.cache_hash);
    let before =
        read_action(root, &section.result.task_id, &section.result.cache_hash)?.map(|(raw, _)| raw);
    let mut plan = lk_vault::MutationPlan::default();
    current.prepared.guard_pages(&mut plan);
    match response {
        crate::commands::execution::finalization::SectionResponse::NeedsAction(reason) => {
            let action = Action {
                schema: SCHEMA.into(),
                task_id: section.result.task_id,
                cache_hash: section.result.cache_hash,
                context_revision: current.prepared.revision(config_revision)?,
                request_revision: digest(&section.request)?,
                reason: reason.clone(),
            };
            plan.publish_expected(
                root,
                path,
                before,
                serde_json::to_string(&action).into_diagnostic()?,
            )
            .into_diagnostic()?;
            session.apply(plan).into_diagnostic()?;
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
            super::guard_queue(
                root,
                config,
                config_revision,
                &result.task_id,
                value.clone(),
                &mut plan,
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
            session.apply(plan).into_diagnostic()?;
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
            assert_staged_review_scope(bounded);
        }
    }

    #[cfg(unix)]
    fn assert_staged_review_scope(bounded: bool) {
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
            let prepared = super::super::resources::task(&root, &config, &task.task_id).unwrap();
            let id = {
                let session = lk_vault::MutationSession::acquire(&root).unwrap();
                let work = super::super::persist_prepared(&session, &config, &revision, &prepared)
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
            serde_json::from_slice(&std::fs::read(root.join(path)).unwrap()).unwrap();
        assert_eq!(retained.context_revision, result.context_revision);
    }
}
