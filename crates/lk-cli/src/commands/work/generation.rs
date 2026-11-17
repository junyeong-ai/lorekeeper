use lk_queue::{QueueTask, TaskKind};
use miette::IntoDiagnostic;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

use super::action::{Action, GENERATION, digest};
use super::{Prepared, Subject};
mod owners;
pub(super) mod rejection;
pub(super) use owners::Owners;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    status: String,
    value: Option<Value>,
    reason: Option<String>,
}

pub(in crate::commands) enum Outcome {
    Complete(Value),
    NeedsAction(String),
    NeedsDiscovery(super::discovery::recovery::Recovery, String),
    Rejected(rejection::ProposalRejection),
}

pub(in crate::commands) enum SelectedFinish {
    Staged,
    NeedsAction(String),
    ScopeChanged,
    DiscoveryRecorded,
    Rejected(String),
}

pub(in crate::commands) fn schema() -> Value {
    let admission = json!({"anyOf":[
        {"type":"object","additionalProperties":false,"properties":{"decision":{"type":"string","enum":["new"]}},"required":["decision"]},
        {"type":"object","additionalProperties":false,"properties":{"decision":{"type":"string","enum":["reuse"]},"owner_ref":{"type":"string"}},"required":["decision","owner_ref"]}
    ]});
    let concept = json!({"type":"object","additionalProperties":false,"properties":{
        "admission":admission,"name":{"type":"string"},"category":{"type":["string","null"]},
        "aliases":{"type":"array","items":{"type":"string"}},"synthesis":{"type":["string","null"]},
        "rss_evidence":{"type":"array","items":{"type":"object","additionalProperties":false,
            "properties":{"event_id":{"type":"string"},"revision":{"type":"string"}},"required":["event_id","revision"]}}},
        "required":["admission","name","category","aliases","synthesis","rss_evidence"]});
    let value = json!({"anyOf":[{"type":"null"},
        {"type":"object","additionalProperties":false,"properties":{"concepts":{"type":"array","items":concept}},"required":["concepts"]},
        {"type":"object","additionalProperties":false,"properties":{"synthesis":{"type":"string"},"related":{"type":["string","null"]}},"required":["synthesis","related"]},
        {"type":"object","additionalProperties":false,"properties":{"queries":{"type":"array","minItems":1,"maxItems":16,"items":{
            "type":"object","additionalProperties":false,"properties":{"query":{"type":"string","minLength":1},"phrase":{"type":"boolean"},"spacing":{"type":"boolean"}},"required":["query","phrase","spacing"]}}},"required":["queries"]}]});
    json!({"type":"object","additionalProperties":false,"properties":{"results":{"type":"array","items":{
        "type":"object","additionalProperties":false,"properties":{
            "status":{"type":"string","enum":["complete","needs-action","needs-discovery"]},"value":value,"reason":{"type":["string","null"]}},
        "required":["status","value","reason"]}}},"required":["results"]})
}

pub(in crate::commands) fn replies(value: Value, count: usize) -> miette::Result<Vec<Value>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Replies {
        results: Vec<Value>,
    }
    let reply: Replies = serde_json::from_value(value).into_diagnostic()?;
    if reply.results.len() != count {
        return Err(miette::miette!(
            "native result count differs from the exact selected tasks"
        ));
    }
    Ok(reply.results)
}

pub(super) fn assemble(
    task: &QueueTask,
    subject: &Subject,
    binding: &super::resources::ResultBinding,
    context_revision: &str,
    owners: &Owners,
    reply: Value,
    max_result_bytes: usize,
) -> miette::Result<Outcome> {
    for key in ["status", "value", "reason"] {
        if reply.get(key).is_none() {
            return Err(miette::miette!("missing native result field: {key}"));
        }
    }
    let reply: Reply = serde_json::from_value(reply).into_diagnostic()?;
    let semantic = match (reply.status.as_str(), reply.value, reply.reason) {
        ("needs-action", None, Some(reason)) if !reason.trim().is_empty() => {
            return Ok(Outcome::NeedsAction(reason));
        }
        ("needs-discovery", Some(value), Some(reason))
            if task.kind == TaskKind::ExtractConcepts && !reason.trim().is_empty() =>
        {
            let recovery: super::discovery::recovery::Recovery =
                serde_json::from_value(value).into_diagnostic()?;
            return Ok(Outcome::NeedsDiscovery(recovery.validate()?, reason));
        }
        ("complete", Some(value), None) if value.is_object() => value,
        _ => {
            return Err(miette::miette!(
                "native result has conflicting or unknown states"
            ));
        }
    };
    let Subject::Queue {
        task_id,
        cache_hash,
        target,
        target_revision,
    } = subject
    else {
        return Err(miette::miette!(
            "native generation requires a queue subject"
        ));
    };
    if task.task_id != *task_id
        || task.cache_hash != *cache_hash
        || task.target.vault_path != *target
    {
        return Err(miette::miette!(
            "native result task differs from prepared scope"
        ));
    }
    let (result, body_error) = match task.kind {
        TaskKind::ExtractConcepts => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Extraction {
                concepts: Vec<NativeConcept>,
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct NativeConcept {
                admission: owners::Admission,
                name: String,
                category: Option<String>,
                aliases: Vec<String>,
                synthesis: Option<String>,
                rss_evidence: Vec<lk_core::rss_evidence::RssEventSelection>,
            }
            let value: Extraction = serde_json::from_value(semantic).into_diagnostic()?;
            let concepts = value
                .concepts
                .into_iter()
                .map(|concept| {
                    Ok(lk_queue::ReportedConcept {
                        admission: owners.resolve(concept.admission)?,
                        concept: lk_core::concept::ExtractedConcept {
                            name: concept.name,
                            category: concept.category,
                            aliases: concept.aliases,
                        },
                        synthesis: concept.synthesis,
                        rss_evidence: concept.rss_evidence,
                    })
                })
                .collect::<Result<Vec<_>, rejection::ProposalRejection>>();
            let concepts = match concepts {
                Ok(concepts) => concepts,
                Err(reason) => return Ok(Outcome::Rejected(reason)),
            };
            let body_error = concepts
                .iter()
                .filter_map(|concept| concept.synthesis.as_deref())
                .find_map(|body| lk_core::markdown::validate_section_body(body).err());
            let result = serde_json::to_value(lk_queue::TaskResult {
                concept_policy: serde_json::from_value(task.input["concept_policy"].clone())
                    .into_diagnostic()?,
                task_id: task_id.clone(),
                cache_hash: cache_hash.clone(),
                target_revision: target_revision.clone(),
                context_revision: context_revision.into(),
                discovery: binding.discovery.clone(),
                selection: binding.selection.clone(),
                target: task.target.clone(),
                date: serde_json::from_value(task.input["date"].clone()).into_diagnostic()?,
                concepts,
            })
            .into_diagnostic()?;
            (result, body_error)
        }
        TaskKind::SynthesizeConcept => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Synthesis {
                synthesis: String,
                related: Option<String>,
            }
            if semantic.get("related").is_none() {
                return Err(miette::miette!(
                    "native synthesis requires explicit related body or null"
                ));
            }
            let value: Synthesis = serde_json::from_value(semantic).into_diagnostic()?;
            let body_error = std::iter::once(value.synthesis.as_str())
                .chain(value.related.as_deref())
                .find_map(|body| lk_core::markdown::validate_section_body(body).err());
            let result = serde_json::to_value(lk_queue::SynthesisResult {
                task_id: task_id.clone(),
                cache_hash: cache_hash.clone(),
                target_revision: target_revision.clone(),
                context_revision: context_revision.into(),
                synthesis: value.synthesis,
                related: value.related,
            })
            .into_diagnostic()?;
            (result, body_error)
        }
        _ => return Err(miette::miette!("task uses the section generation contract")),
    };
    if serde_json::to_vec(&result).into_diagnostic()?.len() > max_result_bytes {
        return Err(miette::miette!(
            "assembled native result exceeds max_result_bytes"
        ));
    }
    if let Some(error) = body_error {
        return Ok(Outcome::NeedsAction(format!(
            "invalid section body: {error}"
        )));
    }
    Ok(Outcome::Complete(result))
}

pub(super) fn assemble_prepared(
    config: &lk_core::config::Config,
    task: &QueueTask,
    prepared: &Prepared,
    revision: &str,
    reply: Value,
    limit: usize,
) -> miette::Result<Outcome> {
    assemble(
        task,
        &prepared.subject,
        &prepared.result_binding()?,
        revision,
        &Owners::build(config, prepared)?,
        reply,
        limit,
    )
}

#[derive(Debug)]
pub(in crate::commands) struct ResultCapacity {
    pub(in crate::commands) minimum_bytes: usize,
    pub(in crate::commands) max_result_bytes: usize,
}

impl std::fmt::Display for ResultCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "native result envelope requires at least {} bytes, above max_result_bytes {}; retained for explicit capacity adjustment",
            self.minimum_bytes, self.max_result_bytes
        )
    }
}
impl std::error::Error for ResultCapacity {}
impl miette::Diagnostic for ResultCapacity {}

pub(super) fn response_limit(
    task: &QueueTask,
    subject: &Subject,
    binding: &super::resources::ResultBinding,
    revision: &str,
    owners: &Owners,
    limit: usize,
) -> miette::Result<usize> {
    let reply = minimum_reply(task)?;
    let Outcome::Complete(result) = assemble(
        task,
        subject,
        binding,
        revision,
        owners,
        reply.clone(),
        usize::MAX,
    )?
    else {
        unreachable!("minimum complete result")
    };
    let reply_bytes = serde_json::to_vec(&reply).into_diagnostic()?.len();
    let metadata = serde_json::to_vec(&result)
        .into_diagnostic()?
        .len()
        .saturating_sub(reply_bytes);
    let minimum_bytes = metadata
        + serde_json::to_vec(&json!({"results":[reply]}))
            .into_diagnostic()?
            .len();
    if minimum_bytes > limit {
        return Err(ResultCapacity {
            minimum_bytes,
            max_result_bytes: limit,
        }
        .into());
    }
    if task.kind == TaskKind::ExtractConcepts {
        owners.response_limit(limit - metadata, reply_bytes)
    } else {
        Ok(limit - metadata)
    }
}

/// Appended to a finalizer's instructions when its request discloses a candidate roster.
pub(super) const ROSTER_INSTRUCTION: &str = "discovery lists each recorded query with the pages it matched and how many of them the ranked candidate roster carried. Pages beyond that roster were never examined: neither the roster nor the owners list is the complete related knowledge, and needs-discovery remains the way to widen the search.";

pub(super) fn minimum_reply(task: &QueueTask) -> miette::Result<Value> {
    let value = match task.kind {
        TaskKind::ExtractConcepts => json!({"concepts":[]}),
        TaskKind::SynthesizeConcept => json!({
            "synthesis":"x",
            "related":(task.anchors().len() == 2).then_some(""),
        }),
        _ => return Err(miette::miette!("task uses section response bounds")),
    };
    Ok(json!({"status":"complete","value":value,"reason":null}))
}

pub(super) fn instruction() -> String {
    format!(
        "Return one results entry for each supplied task, in exact supplied order (one entry for a completed Work). Each entry is {{status: complete, value: semantic_object, reason: null}}, or {{status: needs-action, value: null, reason: nonblank}}. Extraction alone may return {{status: needs-discovery, value: {{queries: [{{query: explicit_query, phrase: boolean, spacing: boolean}}]}}, reason: nonblank}} when new lexical discovery is needed. Supply 1..16 explicit queries, never paths, task IDs or revisions. These queries replace the entire current discovery query list; they are not appended. Supply the complete intended search scope, including current queries still needed for this extraction. Omitted queries will not be carried forward. Rust validates the resulting scope afresh; changed queries alone do not establish that the missing semantic evidence has been obtained. This records an incomplete recovery request; it grants no semantic approval or completion. Extraction semantic_object contains concepts with semantic name/category/aliases/synthesis/rss_evidence fields and admission {{decision: new}} or {{decision: reuse, owner_ref: exact_reference}}. Each task or completed Work supplies its own owners roster. Reuse only a reference from that exact roster after the required identity comparison. Never return owner paths or revisions; Rust expands the selected reference into the unchanged canonical ReportedConcept owner. An empty owners roster permits no reuse; synthesis contains only synthesis and related. Rust supplies all TaskResult/SynthesisResult task/cache/target/context/date/discovery/selection metadata and owns result-file serialization, finish, seals, staging and publication. Never return metadata, source_questions fields or extra keys. {} After completing the required reads and admission judgments, an empty extraction is valid when no concept satisfies the admission policy, even when the evidence leaves source questions open. Never invent a concept to carry unrelated questions. An inspection roster establishes read coverage, not semantic support or approval to reuse an owner. Evidence mappings identify cited resources, not every inspected resource. If a required identity judgment or supporting evidence is absent from the observations, perform the permitted targeted read or return needs-action; neither roster membership nor omission from the citation mapping settles that judgment. Read the supplied scope/contracts completely. Exact owner/name/alias checks, targeted missing reads and review/repair capabilities remain available when necessary. Do not repeat setup/queue enumeration or reconstruct guards. Native revisions are opaque and domain-specific: compare only the same named field from native output, never a raw checksum or another revision field. Rust validates captured revisions. If extraction needs new discovery, use needs-discovery with explicit queries. Other scope changes require needs-action for fresh readiness; never attach new hashes to an old answer. Unknown semantic support is not approval. Processing blockers, missing required reads or actions require needs-action. Honor max_response_bytes, which reserves the native result envelope inside max_result_bytes. Do not drop task-relevant evidence or uncertainty to fit; return needs-action instead. Native guards revalidate the complete current scope after the host and again at publication.",
        crate::commands::execution::finalization::publication_rules()
    )
}

pub(super) enum Admission {
    Batch,
    Single,
    Deferred(String),
}

pub(super) fn admission(
    root: &Path,
    config: &lk_core::config::Config,
    input: &Value,
) -> miette::Result<Admission> {
    let task: QueueTask = crate::commands::queue::current_task(
        root,
        input["task"]["task_id"]
            .as_str()
            .ok_or_else(|| miette::miette!("missing selected task"))?,
    )?;
    let request = super::native::selected_request(config, std::slice::from_ref(input))?;
    let request_revision = digest(&request)?;
    let action = GENERATION.read(root, &task.task_id, &task.cache_hash)?;
    if action.as_ref().is_some_and(|(_, action)| {
        action.request_revision == request_revision
            && Some(action.context_revision.as_str()) != input["context_revision"].as_str()
    }) {
        return Err(miette::miette!(
            "native action context differs from its request"
        ));
    }
    let revision = input["context_revision"]
        .as_str()
        .ok_or_else(|| miette::miette!("missing native generation input revision"))?;
    let rejection = rejection::held(root, &task, revision)?;
    let history = match super::discovery::recovery::admission(root, &task, &request)? {
        super::discovery::recovery::Admission::Blocked(reason) => {
            return Ok(Admission::Deferred(reason));
        }
        super::discovery::recovery::Admission::Changed => true,
        super::discovery::recovery::Admission::New => false,
    };
    if let Some(reason) = rejection {
        return Ok(Admission::Deferred(reason));
    }
    let Some((_, action)) = action else {
        return Ok(if history {
            Admission::Single
        } else {
            Admission::Batch
        });
    };
    if action.request_revision != request_revision {
        return Ok(Admission::Single);
    }
    Ok(Admission::Deferred(action.reason))
}

pub(in crate::commands) fn plan_prune(
    root: &Path,
    pending: &std::collections::BTreeSet<(String, String)>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    Ok(GENERATION.plan_prune(root, pending, plan)? + rejection::plan_prune(root, pending, plan)?)
}

/// The recipes a selected extraction input was prepared under: its disclosed roster
/// coverage when screened, its context's own recipes otherwise.
fn input_queries(input: &Value) -> miette::Result<Vec<lk_core::discovery::DiscoveryQuery>> {
    match input.get("discovery").and_then(Value::as_array) {
        Some(coverage) => coverage
            .iter()
            .map(|row| serde_json::from_value(row["query"].clone()).into_diagnostic())
            .collect(),
        None => serde_json::from_value(input["context"]["purpose"]["discovery"]["queries"].clone())
            .into_diagnostic(),
    }
}

/// The selected task's fresh inputs, or `None` when its discovery, candidate roster or captured
/// scope changed after `input` was prepared; a changed scope never relabels an answer to the old
/// input.
fn current_selected(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    input: &Value,
    read: &lk_vault::ReadControl,
) -> miette::Result<Option<(QueueTask, Prepared, super::Work)>> {
    let task_id = input["task"]["task_id"]
        .as_str()
        .ok_or_else(|| miette::miette!("missing selected task"))?;
    let task = crate::commands::queue::current_task_controlled(root, task_id, read)?;
    if task.kind == TaskKind::ExtractConcepts {
        let queries = input_queries(input)?;
        if super::discovery::current(root, &task)?.is_some_and(|current| current != queries) {
            return Ok(None);
        }
    }
    let prepared = match super::resources::refresh_controlled(
        root,
        config,
        config_revision,
        &input["context"],
        read,
    ) {
        Ok(prepared) => prepared,
        Err(error) if super::is_semantic_context_changed(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let prepared = match super::with_current_roster(root, config, config_revision, &prepared, read)?
    {
        super::RosterAdmission::Bound(bound) => (*bound).into_owned(),
        super::RosterAdmission::Changed(_) => return Ok(None),
    };
    let work = super::expected(root, &prepared, config, config_revision)?;
    let owner =
        lk_vault::resolve_page_owner(root, Path::new(&task.target.vault_path)).into_diagnostic()?;
    let current = super::native::selected_task(config, &task, &prepared, &work, &owner)?;
    Ok((current == *input).then_some((task, prepared, work)))
}

/// Holds one task of a selected request whose tool-enabled host failed and returns the task's
/// hold; `None` when its inputs changed, so a fresh request replaces the failed one.
pub(in crate::commands) fn hold_selected_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    request: &Value,
    input: &Value,
    failure: super::state::InvocationFailure,
    read: &lk_vault::ReadControl,
) -> miette::Result<Option<String>> {
    let root = session.root();
    let Some((task, prepared, work)) =
        current_selected(root, config, config_revision, input, read)?
    else {
        return Ok(None);
    };
    let mut plan = lk_vault::MutationPlan::default();
    prepared.guard_pages(&mut plan);
    let reason = rejection::plan_record(
        root,
        rejection::Binding {
            task: &task,
            work: &work,
            input: rejection::Input::Selected,
            request,
        },
        rejection::Cause::Invocation { failure },
        &mut plan,
    )?;
    session.apply(plan).into_diagnostic()?;
    Ok(Some(format!("{}: {reason}", task.task_id)))
}

pub(in crate::commands) fn finish_selected_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    request: &Value,
    input: &Value,
    reply: Value,
    read: &lk_vault::ReadControl,
) -> miette::Result<SelectedFinish> {
    let root = session.root();
    let Some((task, prepared, work)) =
        current_selected(root, config, config_revision, input, read)?
    else {
        return Ok(SelectedFinish::ScopeChanged);
    };
    let before = GENERATION
        .read(root, &task.task_id, &task.cache_hash)?
        .map(|(raw, _)| raw);
    let path = GENERATION.path(&task.task_id, &task.cache_hash);
    let mut plan = lk_vault::MutationPlan::default();
    let reason = match assemble_prepared(
        config,
        &task,
        &prepared,
        &work.plan.subject_revision,
        reply.clone(),
        work.plan.max_result_bytes,
    )? {
        Outcome::Rejected(reason) => {
            prepared.guard_pages(&mut plan);
            let reason = rejection::plan_record(
                root,
                rejection::Binding {
                    task: &task,
                    work: &work,
                    input: rejection::Input::Selected,
                    request,
                },
                rejection::Cause::proposal(reply, reason)?,
                &mut plan,
            )?;
            SelectedFinish::Rejected(reason)
        }
        Outcome::NeedsDiscovery(recovery, reason) => {
            super::discovery::recovery::record(
                session,
                super::discovery::recovery::Binding {
                    task: &task,
                    prepared: &prepared,
                    request,
                    context_revision: &work.plan.subject_revision,
                    work_id: &work.plan.id,
                },
                recovery,
                reason,
                plan,
            )?;
            return Ok(SelectedFinish::DiscoveryRecorded);
        }
        Outcome::NeedsAction(reason) => {
            prepared.guard_pages(&mut plan);
            plan.publish_expected(
                root,
                path,
                before,
                serde_json::to_string(&Action {
                    schema: GENERATION.schema.into(),
                    task_id: task.task_id.clone(),
                    cache_hash: task.cache_hash.clone(),
                    context_revision: work.plan.subject_revision.clone(),
                    request_revision: digest(request)?,
                    reason: reason.clone(),
                })
                .into_diagnostic()?,
            )
            .into_diagnostic()?;
            SelectedFinish::NeedsAction(reason)
        }
        Outcome::Complete(result) => {
            prepared.guard_pages(&mut plan);
            let mut guards = lk_vault::MutationPlan::default();
            if let Err(error) = super::guard_queue_controlled(
                root,
                config,
                config_revision,
                &task.task_id,
                result.clone(),
                prepared.roster.as_ref().map(|roster| &roster.seal),
                &mut guards,
                prepared.read_control(),
            ) {
                let Some(refusal) = error
                    .downcast_ref::<crate::commands::queue::application::PublicationRejection>()
                else {
                    return Err(error);
                };
                let rejection = rejection::ProposalRejection::PublicationGuard {
                    kind: refusal.kind,
                    detail: refusal.detail.clone(),
                };
                let reason = rejection::plan_record(
                    root,
                    rejection::Binding {
                        task: &task,
                        work: &work,
                        input: rejection::Input::Selected,
                        request,
                    },
                    rejection::Cause::proposal(reply, rejection)?,
                    &mut plan,
                )?;
                session.apply(plan).into_diagnostic()?;
                return Ok(SelectedFinish::Rejected(reason));
            }
            plan.extend(guards);
            let destination = super::queue_result_path(task.kind, &result)?;
            let raw = serde_json::to_string(&result).into_diagnostic()?;
            let prior = match std::fs::read_to_string(root.join(&destination)) {
                Ok(raw) => Some(raw),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).into_diagnostic(),
            };
            if prior.as_ref().is_some_and(|prior| prior != &raw) {
                return Err(miette::miette!(
                    "competing native result already exists; retained without overwrite"
                ));
            }
            plan.publish_expected(root, destination, prior, raw)
                .into_diagnostic()?;
            if let Some(before) = before {
                plan.remove(path, before);
            }
            SelectedFinish::Staged
        }
    };
    session.apply(plan).into_diagnostic()?;
    Ok(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_semantic_body_guards_preserve_bounds_and_valid_markdown() {
        for field in ["synthesis", "related", "grounding"] {
            let kind = if field == "grounding" {
                TaskKind::ExtractConcepts
            } else {
                TaskKind::SynthesizeConcept
            };
            let task: QueueTask = serde_json::from_value(json!({
                "task_id":"task", "created_at":"2026-10-24T00:00:00Z", "kind":kind, "cache_hash":"a".repeat(32),
                "target":{"vault_path":"wiki/concepts/subject.md","kind":if field == "grounding" {"document-concepts"} else {"concept-synthesis"},"anchor":"## Synthesis"},
                "input":{"concept_policy":"supported-new","date":"2026-10-24"}
            })).unwrap();
            let subject = Subject::Queue {
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target: task.target.vault_path.clone(),
                target_revision: "b".repeat(64),
            };
            let binding = super::super::resources::ResultBinding {
                discovery: Vec::new(),
                selection: None,
            };
            for (body, valid) in [
                ("## Sibling\nBody.", false),
                ("Parent\n======\nBody.", false),
                ("<!-- Unclosed block", false),
                ("```md\nUnclosed fence", false),
                ("", true),
                ("### Child\nBody.", true),
                ("> ## Quoted heading\n", true),
                ("- ## Nested heading\n", true),
                ("```md\n## Example\n```\n", true),
                ("    ## Indented example\n", true),
            ] {
                let mut value = json!({"synthesis":"Evidence.","related":null});
                if field == "grounding" {
                    value = json!({"concepts":[{"name":"Subject","aliases":[],"category":null,"admission":{"decision":"new"},"synthesis":body,"rss_evidence":[]}]});
                } else {
                    value[field] = json!(body);
                }
                let reply = json!({"status":"complete","value":value,"reason":null});
                let result = assemble(
                    &task,
                    &subject,
                    &binding,
                    &"c".repeat(64),
                    &Owners::default(),
                    reply.clone(),
                    4096,
                )
                .unwrap();
                assert_eq!(
                    matches!(result, Outcome::Complete(_)),
                    valid,
                    "{field}/{body:?}"
                );
                assert!(
                    valid || matches!(result, Outcome::NeedsAction(_)),
                    "{field}/{body:?}"
                );
                assert!(
                    assemble(
                        &task,
                        &subject,
                        &binding,
                        &"c".repeat(64),
                        &Owners::default(),
                        reply,
                        1
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn response_allowance_reserves_native_identity_and_preserves_explicit_evidence() {
        let task: QueueTask = serde_json::from_value(json!({
            "task_id":"task", "created_at":"2026-10-24T00:00:00Z", "kind":"extract-concepts", "cache_hash":"a".repeat(32),
            "target":{"vault_path":"wiki/documents/source.md","kind":"document-concepts","anchor":"## Related Concepts"},
            "input":{"concept_policy":"supported-new","date":"2026-10-24"}
        })).unwrap();
        let subject = Subject::Queue {
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target: task.target.vault_path.clone(),
            target_revision: "b".repeat(64),
        };
        let selection = json!({"policy":"independent-screening-v1","inventory_revision":"c".repeat(64),"decisions_revision":"d".repeat(64)});
        let binding = super::super::resources::ResultBinding {
            discovery: vec![lk_core::discovery::DiscoveryQuery {
                query: "Protocol".into(),
                phrase: true,
                spacing: false,
            }],
            selection: Some(serde_json::from_value(selection.clone()).unwrap()),
        };
        let revision = "e".repeat(64);
        let owners = Owners(vec![lk_queue::ConceptOwner {
            path: "wiki/concepts/protocol.md".into(),
            revision: "f".repeat(64),
        }]);
        let limit = response_limit(&task, &subject, &binding, &revision, &owners, 4096).unwrap();
        assert!(limit < 4096);
        let mut reply = json!({"status":"complete","reason":null,"value":{"concepts":[{
            "name":"Protocol — 좁은 범위", "aliases":[],"category":null,"admission":{"decision":"reuse","owner_ref":"o0"},
            "synthesis":"A source question remains unknown.","rss_evidence":[{"event_id":"rss:exact","revision":"1".repeat(64)}]
        }]}});
        let base = serde_json::to_vec(&reply).unwrap().len();
        reply["value"]["concepts"][0]["synthesis"] = json!("x".repeat(limit - base));
        assert!(serde_json::to_vec(&reply).unwrap().len() <= limit);
        let Outcome::Complete(result) = assemble(
            &task,
            &subject,
            &binding,
            &revision,
            &owners,
            reply.clone(),
            4096,
        )
        .unwrap() else {
            panic!("complete")
        };
        assert_eq!(result["selection"], selection);
        assert_eq!(result["discovery"][0]["query"], "Protocol");
        assert_eq!(
            result["concepts"][0]["rss_evidence"],
            reply["value"]["concepts"][0]["rss_evidence"]
        );
        assert_eq!(
            result["target"],
            serde_json::to_value(&task.target).unwrap()
        );
        assert!(serde_json::to_vec(&result).unwrap().len() <= 4096);
        reply["value"]["task_id"] = json!("forged");
        assert!(assemble(&task, &subject, &binding, &revision, &owners, reply, 4096).is_err());
        assert!(response_limit(&task, &subject, &binding, &revision, &owners, 10).is_err());
    }
}
