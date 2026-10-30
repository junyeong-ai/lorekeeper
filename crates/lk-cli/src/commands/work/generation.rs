use lk_queue::{QueueTask, TaskKind};
use miette::IntoDiagnostic;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

use super::action::{Action, GENERATION, digest};
use super::{Prepared, Subject};

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
}

pub(in crate::commands) enum SelectedFinish {
    Staged,
    NeedsAction(String),
    ScopeChanged,
    DiscoveryRecorded,
}

pub(in crate::commands) fn schema() -> Value {
    let owner = json!({"type":"object","additionalProperties":false,
        "properties":{"path":{"type":"string"},"revision":{"type":"string"}},
        "required":["path","revision"]});
    let admission = json!({"anyOf":[
        {"type":"object","additionalProperties":false,"properties":{"decision":{"type":"string","enum":["new"]}},"required":["decision"]},
        {"type":"object","additionalProperties":false,"properties":{"decision":{"type":"string","enum":["reuse"]},"owner":owner},"required":["decision","owner"]}
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
    context: &Value,
    context_revision: &str,
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
                concepts: Vec<lk_queue::ReportedConcept>,
            }
            let value: Extraction = serde_json::from_value(semantic).into_diagnostic()?;
            let body_error = value
                .concepts
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
                discovery: serde_json::from_value(
                    context["purpose"]["discovery"]["queries"].clone(),
                )
                .into_diagnostic()?,
                selection: context["purpose"]["extraction_scope"]
                    .get("selection")
                    .map(|value| serde_json::from_value(value.clone()).into_diagnostic())
                    .transpose()?,
                target: task.target.clone(),
                date: serde_json::from_value(task.input["date"].clone()).into_diagnostic()?,
                concepts: value.concepts,
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
    task: &QueueTask,
    prepared: &Prepared,
    revision: &str,
    reply: Value,
    limit: usize,
) -> miette::Result<Outcome> {
    assemble(
        task,
        &prepared.subject,
        &prepared.context,
        revision,
        reply,
        limit,
    )
}

#[derive(Debug)]
pub(super) struct ResultCapacity {
    minimum_bytes: usize,
    max_result_bytes: usize,
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
    context: &Value,
    revision: &str,
    limit: usize,
) -> miette::Result<usize> {
    let value = match task.kind {
        TaskKind::ExtractConcepts => json!({"concepts":[]}),
        TaskKind::SynthesizeConcept => json!({"synthesis":"", "related":null}),
        _ => return Err(miette::miette!("task uses section response bounds")),
    };
    let reply = json!({"status":"complete","value":value,"reason":null});
    let Outcome::Complete(result) =
        assemble(task, subject, context, revision, reply.clone(), usize::MAX)?
    else {
        unreachable!("empty complete result")
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
    Ok(limit - metadata)
}

pub(super) fn instruction() -> &'static str {
    "Return one results entry for each supplied task, in exact supplied order (one entry for a completed Work). Each entry is {status: complete, value: semantic_object, reason: null}, or {status: needs-action, value: null, reason: nonblank}. Extraction alone may return {status: needs-discovery, value: {queries: [{query: explicit_query, phrase: boolean, spacing: boolean}]}, reason: nonblank} when new lexical discovery is needed. Supply 1..16 explicit queries, never paths, task IDs or revisions. These queries replace the entire current discovery query list; they are not appended. Supply the complete intended search scope, including current queries still needed for this extraction. Omitted queries will not be carried forward. Rust validates the resulting scope afresh; changed queries alone do not establish that the missing semantic evidence has been obtained. This records an incomplete recovery request; it grants no semantic approval or completion. Extraction semantic_object contains only concepts in the shared ReportedConcept format; synthesis contains only synthesis and related. Rust supplies all TaskResult/SynthesisResult task/cache/target/context/date/discovery/selection metadata and owns result-file serialization, finish, seals, staging and publication. Never return metadata, source_questions fields or extra keys. Preserve source_questions as attributed uncertainty in the semantic prose, never invent answers. Read the supplied scope/contracts completely. Exact owner/name/alias checks, targeted missing reads and review/repair capabilities remain available when necessary. Do not repeat setup/queue enumeration or reconstruct guards. Native revisions are opaque and domain-specific: compare only the same named field from native output, never a raw checksum or another revision field. Rust validates captured revisions. If extraction needs new discovery, use needs-discovery with explicit queries. Other scope changes require needs-action for fresh readiness; never attach new hashes to an old answer. Unknown semantic support is not approval. Processing blockers, missing required reads or actions require needs-action. Honor max_response_bytes, which reserves the native result envelope inside max_result_bytes. Do not drop evidence, unresolved issues or source questions to fit; return needs-action instead. Native guards revalidate the complete current scope after the host and again at publication."
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
    let history = match super::discovery::recovery::admission(root, &task, &request)? {
        super::discovery::recovery::Admission::Blocked(reason) => {
            return Ok(Admission::Deferred(reason));
        }
        super::discovery::recovery::Admission::Changed => true,
        super::discovery::recovery::Admission::New => false,
    };
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

pub(in crate::commands) fn prune(
    session: &lk_vault::MutationSession,
    pending: &std::collections::BTreeSet<(String, String)>,
) -> miette::Result<usize> {
    GENERATION.prune(session, pending)
}

pub(in crate::commands) fn finish_selected(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    request: &Value,
    input: &Value,
    reply: Value,
) -> miette::Result<SelectedFinish> {
    let root = session.root();
    let task_id = input["task"]["task_id"]
        .as_str()
        .ok_or_else(|| miette::miette!("missing selected task"))?;
    let task = crate::commands::queue::current_task(root, task_id)?;
    if task.kind == TaskKind::ExtractConcepts {
        let queries: Vec<lk_core::discovery::DiscoveryQuery> =
            serde_json::from_value(input["context"]["purpose"]["discovery"]["queries"].clone())
                .into_diagnostic()?;
        if super::discovery::current(root, &task)?.is_some_and(|current| current != queries) {
            return Ok(SelectedFinish::ScopeChanged);
        }
    }
    let prepared = super::resources::refresh(root, config, config_revision, &input["context"])?;
    let work = super::expected(root, &prepared, config, config_revision)?;
    let owner =
        lk_vault::resolve_page_owner(root, Path::new(&task.target.vault_path)).into_diagnostic()?;
    let current = super::native::selected_task(config, &task, &prepared, &work, &owner)?;
    if current != *input {
        return Err(miette::miette!(
            "selected generation scope changed; reread and regenerate"
        ));
    }
    let before = GENERATION
        .read(root, &task.task_id, &task.cache_hash)?
        .map(|(raw, _)| raw);
    let path = GENERATION.path(&task.task_id, &task.cache_hash);
    let mut plan = lk_vault::MutationPlan::default();
    let reason = match assemble_prepared(
        &task,
        &prepared,
        &work.plan.subject_revision,
        reply,
        work.plan.max_result_bytes,
    )? {
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
            super::guard_queue(
                root,
                config,
                config_revision,
                &task.task_id,
                result.clone(),
                &mut plan,
            )?;
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
            let context = json!({"purpose":{"discovery":{"queries":[]},"extraction_scope":{}}});
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
                    &context,
                    &"c".repeat(64),
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
                assert!(assemble(&task, &subject, &context, &"c".repeat(64), reply, 1).is_err());
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
        let selection = json!({"policy":"progressive-evidence-v1","inventory_revision":"c".repeat(64),"decisions_revision":"d".repeat(64)});
        let context = json!({"purpose":{"discovery":{"queries":[{"query":"Protocol","phrase":true,"spacing":false}]},"extraction_scope":{"selection":selection}}});
        let revision = "e".repeat(64);
        let limit = response_limit(&task, &subject, &context, &revision, 4096).unwrap();
        assert!(limit < 4096);
        let mut reply = json!({"status":"complete","reason":null,"value":{"concepts":[{
            "name":"Protocol — 좁은 범위", "aliases":[],"category":null,"admission":{"decision":"reuse","owner":{"path":"wiki/concepts/protocol.md","revision":"f".repeat(64)}},
            "synthesis":"A source question remains unknown.","rss_evidence":[{"event_id":"rss:exact","revision":"1".repeat(64)}]
        }]}});
        let base = serde_json::to_vec(&reply).unwrap().len();
        reply["value"]["concepts"][0]["synthesis"] = json!("x".repeat(limit - base));
        assert!(serde_json::to_vec(&reply).unwrap().len() <= limit);
        let Outcome::Complete(result) =
            assemble(&task, &subject, &context, &revision, reply.clone(), 4096).unwrap()
        else {
            panic!("complete")
        };
        assert_eq!(result["selection"], selection);
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
        assert!(assemble(&task, &subject, &context, &revision, reply, 4096).is_err());
        assert!(response_limit(&task, &subject, &context, &revision, 10).is_err());
    }
}
