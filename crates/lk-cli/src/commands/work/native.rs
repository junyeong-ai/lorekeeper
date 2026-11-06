use std::path::Path;

use lk_core::config::Config;
use lk_core::semantic::{SemanticAuthority, SemanticResource};
use lk_queue::{QueueTask, TaskKind, TaskTarget};
use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{Prepared, Subject, Work};

#[derive(serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::commands) enum FinalizationMode {
    Root,
    Frontier,
}

#[derive(serde::Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub(in crate::commands) enum FinalizationAdmissionState {
    Available {
        request_bytes: Option<usize>,
    },
    Finished,
    Replan,
    NeedsAction,
    ReusableSeal,
    ProcessingAction,
    DiscoveryRecovery,
    FrontierUnavailable {
        reason: super::state::FrontierUnavailable,
    },
    PreparedInputCapacity {
        request_bytes: usize,
        max_input_bytes: usize,
    },
    NativeInputCapacity {
        request_bytes: usize,
        max_input_bytes: usize,
        canonical_input_bytes: usize,
        request_components: FinalizationInputBytes,
    },
    NativeResultCapacity {
        minimum_bytes: usize,
        max_result_bytes: usize,
    },
}

#[derive(serde::Serialize)]
pub(in crate::commands) struct FinalizationInputBytes {
    observations: usize,
    context: usize,
    proof: usize,
    inspected_resources: usize,
    rss_evidence_bindings: usize,
    contracts: usize,
    response_schema: usize,
    instructions: usize,
    metadata_envelope: usize,
}

impl FinalizationInputBytes {
    pub(super) fn build(request: &Value, request_bytes: usize) -> miette::Result<Self> {
        let bytes = |value: &Value| {
            serde_json::to_vec(value)
                .map(|raw| raw.len())
                .into_diagnostic()
        };
        let frontier = &request["frontier"];
        let mut components = Self {
            observations: bytes(&frontier["observations"])?,
            context: bytes(&frontier["context"])?,
            proof: bytes(&frontier["proof"])?,
            inspected_resources: bytes(&request["inspected_resources"])?,
            rss_evidence_bindings: bytes(&request["rss_evidence_bindings"])?,
            contracts: bytes(&request["contracts"])?,
            response_schema: bytes(&request["response_schema"])?,
            instructions: bytes(&request["instructions"])?,
            metadata_envelope: 0,
        };
        let values = components.observations
            + components.context
            + components.proof
            + components.inspected_resources
            + components.rss_evidence_bindings
            + components.contracts
            + components.response_schema
            + components.instructions;
        components.metadata_envelope = request_bytes.checked_sub(values).ok_or_else(|| {
            miette::miette!("finalization components exceed the captured request")
        })?;
        Ok(components)
    }
}

#[derive(serde::Serialize)]
pub(in crate::commands) struct FinalizationAdmission {
    work_id: String,
    task_id: String,
    work_subject_revision: String,
    canonical_input_revision: Option<String>,
    completion_revision: Option<String>,
    request_revision: Option<String>,
    mode: FinalizationMode,
    #[serde(flatten)]
    outcome: FinalizationAdmissionState,
}

impl FinalizationAdmission {
    pub(in crate::commands) fn new(
        work: &Work,
        task: &QueueTask,
        mode: FinalizationMode,
        outcome: FinalizationAdmissionState,
    ) -> Self {
        Self {
            work_id: work.plan.id.clone(),
            task_id: task.task_id.clone(),
            work_subject_revision: work.plan.subject_revision.clone(),
            canonical_input_revision: None,
            completion_revision: None,
            request_revision: None,
            mode,
            outcome,
        }
    }

    pub(super) fn with_completion(mut self, completed: &Value) -> miette::Result<Self> {
        if completed["work_id"] != self.work_id {
            return Err(miette::miette!(
                "finalization admission differs from captured Work"
            ));
        }
        let revision = |field: &str| {
            completed[field]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| miette::miette!("finalization admission has no canonical revision"))
        };
        self.canonical_input_revision = Some(revision("input_revision")?);
        self.completion_revision = Some(revision("result_revision")?);
        Ok(self)
    }

    pub(super) fn with_request_revision(mut self, revision: String) -> Self {
        self.request_revision = Some(revision);
        self
    }

    pub(super) fn with_outcome(mut self, outcome: FinalizationAdmissionState) -> Self {
        self.outcome = outcome;
        self
    }
}

struct Captured<'a> {
    subject: &'a Subject,
    context: &'a Value,
    resources: &'a [SemanticResource],
}

struct FinalizationView {
    observation: Value,
    resources: Vec<Value>,
}

impl FinalizationView {
    fn build(work: &Work, observation: &Value) -> miette::Result<Self> {
        let resources = work.inspection_guards()?;
        let object = observation
            .as_object()
            .filter(|object| !object.contains_key("evidence_resources"))
            .ok_or_else(|| miette::miette!("invalid canonical finalization observation"))?;
        let evidence = object
            .get("evidence")
            .and_then(Value::as_array)
            .ok_or_else(|| miette::miette!("canonical finalization evidence is missing"))?;
        let mut indices = Vec::with_capacity(evidence.len());
        let mut seen = std::collections::BTreeSet::new();
        for reference in evidence {
            let index = reference["resource"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| miette::miette!("invalid canonical evidence resource index"))?;
            if resources.get(index) != Some(reference) || !seen.insert(index) {
                return Err(miette::miette!(
                    "canonical evidence differs from the complete inspected roster"
                ));
            }
            indices.push(index);
        }
        let mut observation = object
            .iter()
            .filter(|(key, _)| key.as_str() != "evidence")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        observation.insert("evidence_resources".into(), json!(indices));
        Ok(Self {
            observation: observation.into(),
            resources,
        })
    }
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
        "schema":"native-selected-results-v3", "tasks":selected, "contracts":contracts,
        "max_input_bytes":limits.max_input_bytes,
        "max_result_bytes":limits.max_result_bytes,
        "max_response_bytes":max_response_bytes,
        "response_schema":super::generation::schema(),
        "instructions":super::generation::instruction(),
    }))
}

pub(in crate::commands) fn selected_characters(tasks: &[Value]) -> miette::Result<usize> {
    tasks.iter().try_fold(0usize, |total, task| {
        let resources = task["resources"]
            .as_array()
            .ok_or_else(|| miette::miette!("selected resources are missing"))?;
        resources.iter().try_fold(total, |total, resource| {
            let text = resource["text"]
                .as_str()
                .ok_or_else(|| miette::miette!("selected resource text is missing"))?;
            total
                .checked_add(text.chars().count())
                .ok_or_else(|| miette::miette!("selected reading workload overflow"))
        })
    })
}

fn rss_evidence_bindings(task: &QueueTask, work: &Work) -> miette::Result<Value> {
    let subject: Subject =
        serde_json::from_value(work.context["subject"].clone()).into_diagnostic()?;
    if !matches!(subject, Subject::Queue { task_id, cache_hash, target, .. }
        if task_id == task.task_id && cache_hash == task.cache_hash && target == task.target.vault_path)
        || work.context["purpose"]["task_kind"] != json!(task.kind)
        || work.context["purpose"]["target"] != json!(task.target)
    {
        return Err(miette::miette!(
            "publication bindings belong to another task"
        ));
    }
    let Some(manifest) = task.input.get("rss_original") else {
        if !work.context["purpose"]["parameters"]["rss_evidence"].is_null() {
            return Err(miette::miette!(
                "inspected RSS publication manifest is absent from the retained task"
            ));
        }
        return Ok(json!([]));
    };
    let manifest: lk_core::rss_original::RssOriginalManifest =
        serde_json::from_value(manifest.clone()).into_diagnostic()?;
    let reference = manifest
        .reference()
        .map_err(|error| miette::miette!("{error}"))?;
    if work.context["purpose"]["parameters"]["rss_evidence"]
        != json!({"manifest_revision":reference,"events":manifest.events.len()})
    {
        return Err(miette::miette!(
            "RSS publication manifest differs from inspected Work"
        ));
    }
    let resources = work
        .plan
        .resources
        .iter()
        .enumerate()
        .map(|(index, guard)| (guard.address.as_str(), (index, guard)))
        .collect::<std::collections::BTreeMap<_, _>>();
    manifest
        .events
        .iter()
        .map(|event| {
            let address = format!(
                "{}/events/{}.json",
                lk_core::rss_original::RSS_ORIGINAL_STORE,
                event.revision
            );
            let (resource, guard) = resources.get(address.as_str()).ok_or_else(|| {
                miette::miette!("RSS publication event is absent from inspected Work")
            })?;
            if guard.authority != SemanticAuthority::RssEvent || guard.revision != event.revision {
                return Err(miette::miette!(
                    "RSS publication event differs from its inspected guard"
                ));
            }
            Ok(json!({"resource":resource,"event_id":event.event_id,"revision":event.revision}))
        })
        .collect::<miette::Result<Vec<_>>>()
        .map(Value::Array)
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
    let view = FinalizationView::build(work, completed)?;
    Ok(json!({
        "schema":"native-finalize-results-v4", "context_revision":context_revision,
        "completed":view.observation, "contracts":contracts,
        "inspected_resources":view.resources,
        "rss_evidence_bindings":rss_evidence_bindings(task, work)?,
        "max_input_bytes":work.max_input_bytes, "max_result_bytes":work.plan.max_result_bytes,
        "max_response_bytes":super::generation::response_limit(task, &subject, &work.context, context_revision, work.plan.max_result_bytes)?,
        "response_schema":super::generation::schema(),
        "instructions":format!("Finalize only this exact Work from its complete guarded root observation. inspected_resources is the complete planned read roster with every native resource index and full guard. completed.evidence_resources lists the exact cited subset by resource index into that roster; use it with this Work's finding/question/resolution ranges. Captured context retains its own mappings. No guard or observation content was dropped. input_revision identifies the canonical native observation, not a hash to reconstruct from this model view. The roster is not source content or a semantic identity judgment. rss_evidence_bindings supplies exact retained-manifest event_id/revision selectors and their inspected resource indices; copy selectors only for articles that support the reported concept. Do not reconstruct IDs from URLs, titles or payload filenames, infer support from roster membership, or upgrade legacy page-only evidence. The supplied completed object replaces work result/status setup reads; preceding discovery and full evidence observations must not restart. {}", super::generation::instruction()),
    }))
}

pub(in crate::commands) fn frontier_request(
    config: &Config,
    task: &QueueTask,
    work: &Work,
    frontier: &Value,
) -> miette::Result<Value> {
    if frontier["kind"] != "finalize-frontier"
        || frontier["work_id"] != work.plan.id
        || frontier["context"] != work.context
        || !frontier["observations"].is_array()
        || frontier["input_revision"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Err(miette::miette!(
            "native frontier transport differs from its captured Work"
        ));
    }
    let purpose = &work.context["purpose"];
    let kind: TaskKind = serde_json::from_value(purpose["task_kind"].clone()).into_diagnostic()?;
    let target: TaskTarget = serde_json::from_value(purpose["target"].clone()).into_diagnostic()?;
    if !matches!(
        kind,
        TaskKind::ExtractConcepts | TaskKind::SynthesizeConcept
    ) || purpose["extraction_scope"]["policy"] == "source-proposals-v1"
    {
        return Err(miette::miette!(
            "this Work cannot use native frontier finalization"
        ));
    }
    let subject: Subject =
        serde_json::from_value(work.context["subject"].clone()).into_diagnostic()?;
    let contracts =
        crate::commands::execution::finalization::generation_contracts(config, kind, target.kind)?;
    let view = FinalizationView::build(work, frontier)?;
    Ok(json!({
        "schema":"native-finalize-frontier-v4", "context_revision":work.plan.subject_revision,
        "frontier":view.observation, "contracts":contracts,
        "inspected_resources":view.resources,
        "rss_evidence_bindings":rss_evidence_bindings(task, work)?,
        "max_input_bytes":work.max_input_bytes,"max_result_bytes":work.plan.max_result_bytes,
        "max_response_bytes":super::generation::response_limit(task, &subject, &work.context, &work.plan.subject_revision, work.plan.max_result_bytes)?,
        "response_schema":super::generation::schema(),
        "instructions":format!("Finalize only this exact Work from the supplied complete current frontier. Its observations cover every planned inspection exactly once; no combined reduction or joint verdict has been recorded. Read every observation and evidence mapping. inspected_resources is the complete planned read roster with every native resource index and full guard. frontier.evidence_resources lists the exact cited subset by resource index into that roster; use it with this Work's finding/question/resolution ranges. Captured context retains its own mappings. No guard or observation content was dropped. input_revision identifies the canonical native frontier, not a hash to reconstruct from this model view. The roster is not source content or a semantic identity judgment. rss_evidence_bindings supplies exact retained-manifest event_id/revision selectors and their inspected resource indices; copy selectors only for articles that support the reported concept. Do not reconstruct IDs from URLs, titles or payload filenames, infer support from roster membership, or upgrade legacy page-only evidence. Compare claims only under their declared scope and effective time; an inherited resolution is not authority to resolve another open question. Processing concerns cannot be relabeled as source uncertainty. The frontier replaces work result/status setup reads; do not restart paid inspections or write a reduction output. Rust owns its distinct guarded completion proof and final publication. {}", super::generation::instruction()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::work::state::{WorkFinding, WorkLimits, WorkOutput, WorkQuestion};

    fn config() -> Config {
        lk_core::yaml::from_str(include_str!("../../../../../config.starter.yaml")).unwrap()
    }

    fn canonical_observation(request: &Value, field: &str) -> Value {
        let mut observation = request[field].clone();
        let indices = observation
            .as_object_mut()
            .unwrap()
            .remove("evidence_resources")
            .unwrap();
        observation["evidence"] = indices
            .as_array()
            .unwrap()
            .iter()
            .map(|index| request["inspected_resources"][index.as_u64().unwrap() as usize].clone())
            .collect::<Vec<_>>()
            .into();
        observation
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
    fn selected_reading_workload_counts_complete_multitask_unicode_sources() {
        let selected =
            json!({"resources":[{"text":"a".repeat(40000)},{"text":"가".repeat(20000)}]});
        let before = selected.clone();
        assert_eq!(
            selected_characters(std::slice::from_ref(&selected)).unwrap(),
            60000
        );
        assert_eq!(
            selected_characters(&[selected.clone(), selected.clone()]).unwrap(),
            120000
        );
        assert!(
            selected_characters(&[selected.clone(), selected.clone()]).unwrap()
                > lk_core::semantic::INSPECTION_CHARACTERS.get()
        );
        assert_eq!(selected, before);
        for malformed in [
            json!({}),
            json!({"resources":[{}]}),
            json!({"resources":[{"text":null}]}),
        ] {
            assert!(selected_characters(&[malformed]).is_err());
        }
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
        assert!(
            request["instructions"]
                .as_str()
                .unwrap()
                .contains("replace the entire current discovery query list")
        );
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
        let completed = json!({"kind":"finalize","work_id":work.plan.id,"context":context,"input_revision":"root-input","output":{"summary":"Pinned finding","source_questions":[{"id":"question"}]},"evidence":work.inspection_guards().unwrap()});
        let request = finalization_request(&config(), &task, &work, "scope", &completed).unwrap();
        assert_eq!(canonical_observation(&request, "completed"), completed);
        assert_eq!(
            request["inspected_resources"],
            json!(work.inspection_guards().unwrap())
        );
        assert_eq!(
            request["inspected_resources"][0]["guard"],
            json!(work.plan.resources[0])
        );
        assert!(
            request["instructions"]
                .as_str()
                .unwrap()
                .contains("replace the entire current discovery query list")
        );
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

    #[test]
    fn bounded_finalizers_supply_exact_rss_selectors_from_the_inspected_manifest() {
        use lk_core::rss_original::{RSS_ORIGINAL_SCHEME, RSS_ORIGINAL_STORE};
        let (mut task, _, mut context, mut resources, _, _) = fixture();
        let text = "Pinned original event";
        let revision = lk_core::concept::file_revision(text);
        let date = "2026-10-24".parse().unwrap();
        let event_id = lk_core::event::EventId::new("rss", date, "entry");
        let manifest: lk_core::rss_original::RssOriginalManifest = serde_json::from_value(json!({
            "schema":RSS_ORIGINAL_SCHEME,"source_id":"rss","date":date,
            "events":[{"event_id":event_id,"revision":revision,"bytes":text.len(),
                "event_timestamp":"2026-10-24T00:00:00Z","title":"Original","url":null}]
        }))
        .unwrap();
        task.input.insert("rss_original".into(), json!(manifest));
        context["purpose"]["parameters"]["rss_evidence"] = json!({
            "manifest_revision":manifest.reference().unwrap(),"events":1
        });
        resources.push(SemanticResource {
            address: format!("{RSS_ORIGINAL_STORE}/events/{revision}.json"),
            revision: revision.clone(),
            authority: SemanticAuthority::RssEvent,
            text: text.into(),
        });
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
        let expected = json!([{"resource":1,"event_id":event_id,"revision":revision}]);
        let completed = json!({"kind":"finalize","work_id":work.plan.id,
            "context":context,"input_revision":"root-input","output":{"summary":"Source finding"},
            "evidence":work.inspection_guards().unwrap()});
        let frontier = json!({"kind":"finalize-frontier","work_id":work.plan.id,
            "context":context,"input_revision":"frontier-input","observations":[],
            "evidence":work.inspection_guards().unwrap()});
        let root = finalization_request(&config(), &task, &work, "scope", &completed).unwrap();
        let frontier_request = frontier_request(&config(), &task, &work, &frontier).unwrap();
        for request in [&root, &frontier_request] {
            assert_eq!(request["rss_evidence_bindings"], expected);
            assert!(!serde_json::to_string(request).unwrap().contains(text));
        }
        assert_eq!(canonical_observation(&root, "completed"), completed);
        assert_eq!(
            canonical_observation(&frontier_request, "frontier"),
            frontier
        );
        let mut changed = task.clone();
        changed.input.remove("rss_original");
        assert!(finalization_request(&config(), &changed, &work, "scope", &completed).is_err());
        let mut changed = task.clone();
        changed.input.get_mut("rss_original").unwrap()["events"][0]["revision"] =
            "f".repeat(64).into();
        assert!(finalization_request(&config(), &changed, &work, "scope", &completed).is_err());
        let mut changed = work.clone();
        changed.plan.resources[1].authority = SemanticAuthority::Page;
        assert!(finalization_request(&config(), &task, &changed, "scope", &completed).is_err());
        let (legacy, _, context, _, work, _) = fixture();
        let completed = json!({"kind":"finalize","work_id":work.plan.id,
            "context":context,"input_revision":"root-input","output":{"summary":"Legacy page only"},
            "evidence":work.inspection_guards().unwrap()});
        let legacy = finalization_request(&config(), &legacy, &work, "scope", &completed).unwrap();
        assert_eq!(legacy["rss_evidence_bindings"], json!([]));
    }

    #[test]
    fn finalizer_view_requires_exact_unique_native_evidence_references() {
        let (_, _, _, _, work, _) = fixture();
        let roster = work.inspection_guards().unwrap();
        let canonical = json!({"input_revision":"canonical-domain","evidence":roster,
            "context":{"source_observations":{"work_id":"another-work","evidence":[{"resource":99,"guard":{"revision":"another-native-domain"}}]}},
            "output":{"summary":"Unchanged","source_questions":[],"unresolved":[]}});
        let mut corrupt = Vec::new();
        let mut duplicate = canonical.clone();
        duplicate["evidence"]
            .as_array_mut()
            .unwrap()
            .push(roster[0].clone());
        corrupt.push(duplicate);
        for invalid in [json!(1), json!(-1), json!(false), Value::Null] {
            let mut changed = canonical.clone();
            changed["evidence"][0]["resource"] = invalid;
            corrupt.push(changed);
        }
        let mut changed = canonical.clone();
        changed["evidence"][0]["guard"]["revision"] = "another-domain".into();
        corrupt.push(changed);
        let mut changed = canonical.clone();
        changed["evidence_resources"] = json!([]);
        corrupt.push(changed);
        let mut changed = canonical.clone();
        changed.as_object_mut().unwrap().remove("evidence");
        corrupt.push(changed);
        for invalid in corrupt {
            assert!(FinalizationView::build(&work, &invalid).is_err());
        }
        let mut uncited = canonical.clone();
        uncited["evidence"] = json!([]);
        let view = FinalizationView::build(&work, &uncited).unwrap();
        assert_eq!(view.observation["context"], canonical["context"]);
        assert!(
            view.observation["evidence_resources"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(view.resources, roster);
        assert_eq!(canonical["input_revision"], "canonical-domain");
    }

    #[test]
    fn complete_frontier_fits_finalizer_without_repeated_resource_guards() {
        let (task, _, context, _, _, _) = fixture();
        let root = tempfile::tempdir().unwrap();
        let session = lk_vault::MutationSession::acquire(root.path()).unwrap();
        let resources = (0..384)
            .map(|index| {
                let text = format!("Declared source {index}. {}", "x".repeat(2000));
                SemanticResource {
                    address: format!("wiki/documents/{index:03}-{}.md", "a".repeat(170)),
                    revision: lk_core::concept::file_revision(&text),
                    authority: SemanticAuthority::Page,
                    text,
                }
            })
            .collect::<Vec<_>>();
        let work = Work::create(
            &session,
            &resources,
            "scope",
            context,
            WorkLimits {
                max_input_bytes: 262144,
                max_result_bytes: 32768,
            },
        )
        .unwrap();
        assert!(work.plan.parts.len() > 1);
        for part in &work.plan.parts {
            let input = work.input(root.path(), &part.id, &resources).unwrap();
            let ranges = input["input"]["ranges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|range| serde_json::from_value(range["range"].clone()).unwrap())
                .collect::<Vec<lk_core::semantic::SemanticRange>>();
            work.record(
                &session,
                WorkOutput {
                    work_id: work.plan.id.clone(),
                    node_id: part.id.clone(),
                    input_revision: input["input_revision"].as_str().unwrap().into(),
                    summary: "Every assigned source range was inspected.".into(),
                    findings: ranges
                        .iter()
                        .map(|range| WorkFinding {
                            statement: "보존된 source claim.".into(),
                            scope: Some("Declared scope".into()),
                            effective_time: None,
                            attribution: None,
                            evidence: vec![range.clone()],
                        })
                        .collect(),
                    source_questions: vec![WorkQuestion {
                        id: format!("{}:source:0", part.id),
                        question: "Which independent evidence confirms this claim?".into(),
                        evidence: vec![ranges[0].clone()],
                    }],
                    unresolved: vec![],
                    resolutions: vec![],
                },
                &resources,
            )
            .unwrap();
        }
        assert!(!work.progress(root.path(), &resources).unwrap().complete);
        let completed = work
            .capture_frontier(root.path(), &resources)
            .unwrap()
            .into_completion()
            .unwrap();
        let canonical = completed.result();
        let before = canonical.clone();
        let request = frontier_request(&config(), &task, &work, canonical).unwrap();
        let canonical_bytes = serde_json::to_vec(canonical).unwrap().len();
        let request_bytes = serde_json::to_vec(&request).unwrap().len();
        assert!(canonical_bytes <= work.max_input_bytes);
        assert!(
            request_bytes <= work.max_input_bytes,
            "canonical {canonical_bytes} bytes fits; native finalizer {request_bytes} exceeds {}",
            work.max_input_bytes
        );
        assert_eq!(canonical_observation(&request, "frontier"), before);
        assert_eq!(
            request["inspected_resources"].as_array().unwrap().len(),
            resources.len()
        );
        assert_eq!(
            request["frontier"]["input_revision"],
            canonical["input_revision"]
        );
        assert_eq!(canonical, &before);
        eprintln!("canonical {canonical_bytes} bytes; native finalizer {request_bytes} bytes");
        let exact =
            super::super::FrontierFinalization::build(&work, &task, request, before, request_bytes)
                .unwrap();
        let super::super::FrontierFinalization::Ready { result, admission } = exact else {
            panic!("the exact complete request bound must fit");
        };
        let accepted = serde_json::to_value(admission).unwrap();
        assert_eq!(
            accepted["canonical_input_revision"],
            result.completed["input_revision"]
        );
        assert_eq!(
            accepted["completion_revision"],
            result.completed["result_revision"]
        );
        assert_eq!(
            accepted["request_revision"],
            lk_core::rss_original::digest(&serde_json::to_vec(&result.request).unwrap())
        );
        let overflow = super::super::FrontierFinalization::build(
            &work,
            &task,
            result.request,
            result.completed,
            request_bytes - 1,
        )
        .unwrap();
        let super::super::FrontierFinalization::Unavailable(admission) = overflow else {
            panic!("one byte over the complete request bound must refuse admission");
        };
        let rejected = serde_json::to_value(admission).unwrap();
        assert_eq!(rejected["state"], "native-input-capacity");
        assert_eq!(rejected["request_bytes"], request_bytes);
        assert_eq!(rejected["max_input_bytes"], request_bytes - 1);
        assert_eq!(rejected["canonical_input_bytes"], canonical_bytes);
        let components = rejected["request_components"].as_object().unwrap();
        assert_eq!(components.len(), 9);
        assert_eq!(
            components
                .values()
                .map(|size| size.as_u64().unwrap())
                .sum::<u64>(),
            request_bytes as u64
        );
        for field in [
            "canonical_input_revision",
            "completion_revision",
            "request_revision",
        ] {
            assert_eq!(rejected[field], accepted[field]);
        }
    }

    #[test]
    fn capacity_components_measure_disjoint_json_bytes_without_changing_the_request() {
        let request = json!({
            "frontier": {
                "context": {"purpose":"한글 \"quoted\"\n"},
                "observations":[{"summary":"évidence \\完整"}],
                "proof":{"nodes":[]},
                "evidence_resources":[0],
                "input_revision":"same canonical identity"
            },
            "inspected_resources":[{"address":"한글.md"}],
            "rss_evidence_bindings":[],
            "contracts":["exact rules"],
            "response_schema":{"type":"object"},
            "instructions":"preserve every claim\n",
            "schema":"native-finalize-frontier-v4"
        });
        let raw = serde_json::to_vec(&request).unwrap();
        let components =
            serde_json::to_value(FinalizationInputBytes::build(&request, raw.len()).unwrap())
                .unwrap();
        for (name, value) in [
            ("observations", &request["frontier"]["observations"]),
            ("context", &request["frontier"]["context"]),
            ("proof", &request["frontier"]["proof"]),
            ("inspected_resources", &request["inspected_resources"]),
            ("rss_evidence_bindings", &request["rss_evidence_bindings"]),
            ("contracts", &request["contracts"]),
            ("response_schema", &request["response_schema"]),
            ("instructions", &request["instructions"]),
        ] {
            assert_eq!(components[name], serde_json::to_vec(value).unwrap().len());
        }
        assert_eq!(
            components
                .as_object()
                .unwrap()
                .values()
                .map(|size| size.as_u64().unwrap())
                .sum::<u64>(),
            raw.len() as u64
        );
        assert!(components["metadata_envelope"].as_u64().unwrap() > 0);
        assert_eq!(serde_json::to_vec(&request).unwrap(), raw);
    }
}
