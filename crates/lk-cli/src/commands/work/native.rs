use std::path::Path;

use lk_core::config::Config;
use lk_core::semantic::{SemanticAuthority, SemanticResource};
use lk_queue::{QueueTask, TaskKind, TaskTarget};
use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{Prepared, Subject, Work};

struct Captured<'a> {
    subject: &'a Subject,
    context: &'a Value,
    resources: &'a [SemanticResource],
}

pub(super) fn selected_task(
    config: &Config,
    task: &QueueTask,
    prepared: &Prepared,
    work: &Work,
    target_owner: &Path,
) -> miette::Result<Value> {
    let target = prepared
        .pages
        .get(target_owner)
        .ok_or_else(|| miette::miette!("selected target owner is absent from captured pages"))?;
    selected_task_value(
        config,
        task,
        Captured {
            subject: &prepared.subject,
            context: &prepared.context,
            resources: &prepared.resources,
        },
        work,
        target_owner,
        target,
    )
}

fn selected_task_value(
    config: &Config,
    task: &QueueTask,
    captured: Captured<'_>,
    work: &Work,
    target_owner: &Path,
    target: &str,
) -> miette::Result<Value> {
    let Subject::Queue {
        task_id,
        cache_hash,
        target: declared_target,
        target_revision,
    } = captured.subject
    else {
        return Err(miette::miette!(
            "selected transport requires a queue subject"
        ));
    };
    if task_id != &task.task_id
        || cache_hash != &task.cache_hash
        || declared_target != &task.target.vault_path
        || lk_core::concept::file_revision(target) != *target_revision
        || captured.context != &work.context
    {
        return Err(miette::miette!(
            "selected task differs from its captured scope"
        ));
    }
    work.whole_input_bytes(captured.resources)?;
    let contracts = crate::commands::execution::finalization::generation_contracts(
        config,
        task.kind,
        task.target.kind,
    )?;
    let mut metadata = serde_json::to_value(task).into_diagnostic()?;
    let retained = if let Some(text) = metadata["input"]
        .as_object_mut()
        .ok_or_else(|| miette::miette!("selected task input must be an object"))?
        .remove("text")
    {
        let text = text
            .as_str()
            .ok_or_else(|| miette::miette!("selected retained text must be a string"))?;
        let address = format!(
            "retained-input://{}/{}",
            task.target.vault_path,
            task.target.kind.llm_inputs_key()
        );
        let resource = captured
            .resources
            .iter()
            .find(|resource| resource.address == address)
            .ok_or_else(|| miette::miette!("selected retained input is absent from resources"))?;
        if resource.authority != SemanticAuthority::RetainedInput || resource.text != text {
            return Err(miette::miette!(
                "selected retained input differs from its captured resource"
            ));
        }
        Some(
            json!({"address":resource.address,"revision":resource.revision,"authority":resource.authority}),
        )
    } else {
        None
    };
    let owner = target_owner
        .to_str()
        .ok_or_else(|| miette::miette!("selected owner is not UTF-8"))?;
    let mut resources = captured.resources.to_vec();
    if let Some(resource) = resources.iter().find(|resource| resource.address == owner) {
        if resource.revision != *target_revision || resource.text != target {
            return Err(miette::miette!(
                "selected target resource differs from captured owner"
            ));
        }
    } else {
        resources.push(SemanticResource {
            address: owner.to_owned(),
            revision: target_revision.clone(),
            authority: SemanticAuthority::Subject,
            text: target.to_owned(),
        });
    }
    Ok(json!({
        "kind":"selected-task", "task":metadata,
        "context_revision":work.plan.subject_revision, "context":captured.context,
        "retained_input":retained, "target_resource":owner,
        "resources":resources, "contracts":contracts,
        "max_result_bytes":work.plan.max_result_bytes,
        "max_response_bytes":super::generation::response_limit(task, captured.subject, captured.context, &work.plan.subject_revision, work.plan.max_result_bytes)?,
    }))
}

pub(in crate::commands) fn selected_request(
    config: &Config,
    tasks: &[Value],
) -> miette::Result<Value> {
    if tasks.is_empty() {
        return Err(miette::miette!(
            "selected request requires at least one task"
        ));
    }
    let mut contracts = Vec::new();
    let mut selected = Vec::with_capacity(tasks.len());
    for task in tasks {
        if task["kind"] != "selected-task" {
            return Err(miette::miette!("invalid selected task transport"));
        }
        let mut task = task.clone();
        let rules = task
            .as_object_mut()
            .and_then(|task| task.remove("contracts"))
            .and_then(|rules| rules.as_array().cloned())
            .ok_or_else(|| miette::miette!("selected task contracts are missing"))?;
        let mut indices = Vec::with_capacity(rules.len());
        for rule in rules {
            let index = match contracts.iter().position(|existing| existing == &rule) {
                Some(index) => index,
                None => {
                    contracts.push(rule);
                    contracts.len() - 1
                }
            };
            indices.push(index);
        }
        task["contract_indices"] = json!(indices);
        selected.push(task);
    }
    let limits = super::limits(config)?;
    let max_response_bytes = tasks
        .iter()
        .map(|task| {
            task["max_response_bytes"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| miette::miette!("missing native semantic response allowance"))
        })
        .collect::<miette::Result<Vec<_>>>()?
        .into_iter()
        .min()
        .expect("nonempty selection");
    Ok(json!({
        "schema":"native-selected-results-v2", "tasks":selected, "contracts":contracts,
        "max_input_bytes":limits.max_input_bytes,
        "max_result_bytes":limits.max_result_bytes,
        "max_response_bytes":max_response_bytes,
        "response_schema":super::generation::schema(),
        "instructions":super::generation::instruction(),
    }))
}

pub(in crate::commands) fn finalization_request(
    config: &Config,
    task: &QueueTask,
    work: &Work,
    context_revision: &str,
    completed: &Value,
) -> miette::Result<Value> {
    if context_revision != work.plan.subject_revision
        || completed["kind"] != "finalize"
        || completed["work_id"] != work.plan.id
        || completed["context"] != work.context
        || !completed["output"].is_object()
        || completed["input_revision"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err(miette::miette!(
            "finalizer transport differs from completed work"
        ));
    }
    let purpose = &work.context["purpose"];
    if purpose["extraction_scope"]["policy"] == "source-proposals-v1" {
        return Err(miette::miette!(
            "source observations cannot finalize knowledge extraction"
        ));
    }
    let kind: TaskKind = serde_json::from_value(purpose["task_kind"].clone()).into_diagnostic()?;
    let target: TaskTarget = serde_json::from_value(purpose["target"].clone()).into_diagnostic()?;
    let contracts =
        crate::commands::execution::finalization::generation_contracts(config, kind, target.kind)?;
    let subject: Subject =
        serde_json::from_value(work.context["subject"].clone()).into_diagnostic()?;
    Ok(json!({
        "schema":"native-finalize-results-v1", "context_revision":context_revision,
        "completed":completed, "contracts":contracts,
        "max_input_bytes":work.max_input_bytes, "max_result_bytes":work.plan.max_result_bytes,
        "max_response_bytes":super::generation::response_limit(task, &subject, &work.context, context_revision, work.plan.max_result_bytes)?,
        "response_schema":super::generation::schema(),
        "instructions":format!("Finalize only this exact Work from its complete guarded root observation and evidence mapping. The supplied completed object replaces work result/status setup reads; preceding discovery and full evidence observations must not restart. {}", super::generation::instruction()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::work::state::WorkLimits;

    fn config() -> Config {
        lk_core::yaml::from_str(include_str!("../../../../../config.starter.yaml")).unwrap()
    }

    fn fixture() -> (
        QueueTask,
        Subject,
        Value,
        Vec<SemanticResource>,
        Work,
        String,
    ) {
        let text = "Exact retained source, distinct from the current target.";
        let target = "---\ntype: document\n---\n## Related Concepts\n".to_owned();
        let task: QueueTask = serde_json::from_value(json!({
            "task_id":"task", "kind":"extract-concepts", "created_at":"2026-10-24T00:00:00Z",
            "cache_hash":"hash", "input":{"text":text,"locale":"ko","source_type":"manual","concept_policy":"supported-new","date":"2026-10-24"},
            "target":{"vault_path":"wiki/documents/alias.md","anchor":"## Related Concepts","kind":"document-concepts"},
        })).unwrap();
        let subject = Subject::Queue {
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target: task.target.vault_path.clone(),
            target_revision: lk_core::concept::file_revision(&target),
        };
        let context = json!({"subject":subject,"purpose":{"operation":"queue","task_kind":task.kind,"target":task.target,"discovery":{"queries":[]},"parameters":{"locale":"ko","source_type":"manual"}}});
        let resources = vec![SemanticResource {
            address: "retained-input://wiki/documents/alias.md/concepts".into(),
            revision: lk_core::concept::file_revision(text),
            authority: SemanticAuthority::RetainedInput,
            text: text.into(),
        }];
        let work = Work::build(
            &resources,
            "scope",
            context.clone(),
            WorkLimits {
                max_input_bytes: 262144,
                max_result_bytes: 32768,
            },
        )
        .unwrap();
        (task, subject, context, resources, work, target)
    }

    #[test]
    fn selected_transport_retains_exact_guards_and_supplies_source_once() {
        let (task, subject, context, resources, work, target) = fixture();
        let value = selected_task_value(
            &config(),
            &task,
            Captured {
                subject: &subject,
                context: &context,
                resources: &resources,
            },
            &work,
            Path::new("wiki/documents/owner.md"),
            &target,
        )
        .unwrap();
        assert_eq!(value["context_revision"], "scope");
        assert_eq!(value["context"], context);
        assert!(value["task"]["input"].get("text").is_none());
        assert_eq!(value["retained_input"]["authority"], "retained-input");
        assert_eq!(value["target_resource"], "wiki/documents/owner.md");
        assert_eq!(value["resources"][1]["text"], target);
        let serialized = serde_json::to_string(&value).unwrap();
        assert_eq!(serialized.matches(&resources[0].text).count(), 1);
        assert_eq!(work.context, context);
        assert!(
            selected_task_value(
                &config(),
                &task,
                Captured {
                    subject: &subject,
                    context: &context,
                    resources: &resources
                },
                &work,
                Path::new("wiki/documents/owner.md"),
                "changed"
            )
            .is_err()
        );
    }

    #[test]
    fn selected_envelope_deduplicates_complete_contracts_without_clipping() {
        let config = config();
        let (task, subject, context, resources, work, target) = fixture();
        let selected = selected_task_value(
            &config,
            &task,
            Captured {
                subject: &subject,
                context: &context,
                resources: &resources,
            },
            &work,
            Path::new("wiki/documents/owner.md"),
            &target,
        )
        .unwrap();
        let count = selected["contracts"].as_array().unwrap().len();
        let request = selected_request(&config, &[selected.clone(), selected]).unwrap();
        assert_eq!(request["contracts"].as_array().unwrap().len(), count);
        assert_eq!(
            request["tasks"][0]["contract_indices"],
            request["tasks"][1]["contract_indices"]
        );
        assert!(request["tasks"][0].get("contracts").is_none());
        assert!(selected_request(&config, &[]).is_err());
    }

    #[test]
    fn finalizer_keeps_root_and_publication_guards_distinct_without_source_bodies() {
        let (task, _, context, resources, work, _) = fixture();
        let completed = json!({"kind":"finalize","work_id":work.plan.id,"context":context,"input_revision":"root-input","output":{"summary":"Pinned finding","source_questions":[{"id":"question"}]},"evidence":work.plan.resources});
        let request = finalization_request(&config(), &task, &work, "scope", &completed).unwrap();
        assert_eq!(request["completed"], completed);
        assert_eq!(request["context_revision"], "scope");
        assert_eq!(request["completed"]["input_revision"], "root-input");
        assert!(
            !serde_json::to_string(&request)
                .unwrap()
                .contains(&resources[0].text)
        );
        assert!(
            finalization_request(&config(), &task, &work, "another-scope", &completed).is_err()
        );
    }
}
