use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lk_core::discovery::DiscoveryQuery;
use lk_queue::{QueueTask, TaskKind};
use lk_vault::MutationPlan;
#[cfg(unix)]
use lk_vault::MutationSession;
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use serde_json::Value;

#[cfg(unix)]
use super::super::{Prepared, Subject};

const SCHEMA: &str = "extraction-discovery-recovery-v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(unix)]
pub(in crate::commands) struct Recovery {
    pub queries: Vec<DiscoveryQuery>,
}

#[cfg(unix)]
impl Recovery {
    pub fn validate(self) -> miette::Result<Self> {
        let queries = super::validate_queries(&self.queries)?;
        if queries.is_empty() {
            return Err(miette::miette!(
                "discovery recovery requires explicit queries"
            ));
        }
        Ok(Self { queries })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct History {
    schema: String,
    task_id: String,
    cache_hash: String,
    receipts: Vec<Receipt>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    receipt_revision: String,
    request_revision: String,
    context_revision: String,
    work_id: String,
    target_revision: String,
    recipe_revision: String,
    queries: Vec<DiscoveryQuery>,
    reason: String,
    disposition: Disposition,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Disposition {
    Changed,
    Unchanged,
}

impl Receipt {
    fn revision(&self, task_id: &str, cache_hash: &str) -> miette::Result<String> {
        revision(&(
            SCHEMA,
            task_id,
            cache_hash,
            &self.request_revision,
            &self.context_revision,
            &self.work_id,
            &self.target_revision,
            &self.recipe_revision,
            &self.queries,
            &self.reason,
            &self.disposition,
        ))
    }
}

pub(super) fn path(task_id: &str) -> PathBuf {
    super::path(task_id).with_extension("recovery.json")
}

fn revision(value: &impl Serialize) -> miette::Result<String> {
    Ok(lk_core::concept::file_revision(
        &serde_json::to_string(value).into_diagnostic()?,
    ))
}

fn hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl History {
    fn decode(raw: &str) -> miette::Result<Self> {
        let history: Self = serde_json::from_str(raw).into_diagnostic()?;
        if history.schema != SCHEMA
            || history.task_id.is_empty()
            || history.cache_hash.is_empty()
            || history.receipts.is_empty()
        {
            return Err(miette::miette!("invalid discovery recovery history"));
        }
        let mut seen = BTreeSet::new();
        for receipt in &history.receipts {
            let queries = super::validate_queries(&receipt.queries)?;
            if queries.is_empty()
                || queries != receipt.queries
                || revision(&queries)? != receipt.recipe_revision
                || receipt.reason.trim().is_empty()
                || !hash(&receipt.request_revision)
                || !hash(&receipt.context_revision)
                || !hash(&receipt.target_revision)
                || !hash(&receipt.work_id)
                || receipt.receipt_revision
                    != receipt.revision(&history.task_id, &history.cache_hash)?
                || !seen.insert(&receipt.request_revision)
            {
                return Err(miette::miette!("invalid discovery recovery receipt"));
            }
        }
        Ok(history)
    }
}

fn read(root: &Path, task: &QueueTask) -> miette::Result<Option<(String, History)>> {
    let Some(raw) = super::super::state::read_optional(root, &path(&task.task_id))? else {
        return Ok(None);
    };
    let history = History::decode(&raw)?;
    if task.kind != TaskKind::ExtractConcepts
        || history.task_id != task.task_id
        || history.cache_hash != task.cache_hash
    {
        return Err(miette::miette!(
            "discovery recovery belongs to another task"
        ));
    }
    let recipe = super::super::state::read_optional(root, &super::path(&task.task_id))?
        .ok_or_else(|| miette::miette!("discovery recovery recipe is missing"))?;
    let recipe: super::DiscoveryRecord = serde_json::from_str(&recipe).into_diagnostic()?;
    if recipe.task_id != task.task_id
        || recipe.cache_hash != task.cache_hash
        || !hash(&recipe.target_revision)
    {
        return Err(miette::miette!(
            "discovery recovery recipe has invalid identity"
        ));
    }
    super::validate_queries(&recipe.queries)?;
    Ok(Some((raw, history)))
}

pub(in crate::commands::work) fn has_history(
    root: &Path,
    task: &QueueTask,
) -> miette::Result<bool> {
    Ok(read(root, task)?.is_some())
}

#[cfg(unix)]
pub(in crate::commands::work) enum Admission {
    New,
    Changed,
    Blocked(String),
}

#[cfg(unix)]
pub(in crate::commands::work) fn admission(
    root: &Path,
    task: &QueueTask,
    request: &Value,
) -> miette::Result<Admission> {
    let Some((_, history)) = read(root, task)? else {
        return Ok(Admission::New);
    };
    let request_revision = revision(request)?;
    Ok(history
        .receipts
        .into_iter()
        .find(|receipt| receipt.request_revision == request_revision)
        .map_or(Admission::Changed, |receipt| {
            Admission::Blocked(receipt.reason)
        }))
}

#[cfg(unix)]
pub(in crate::commands::work) fn blocked(
    root: &Path,
    task: &QueueTask,
    request: &Value,
) -> miette::Result<Option<String>> {
    Ok(match admission(root, task, request)? {
        Admission::Blocked(reason) => Some(reason),
        Admission::New | Admission::Changed => None,
    })
}

#[cfg(unix)]
pub(in crate::commands::work) struct Binding<'a> {
    pub task: &'a QueueTask,
    pub prepared: &'a Prepared,
    pub request: &'a Value,
    pub context_revision: &'a str,
    pub work_id: &'a str,
}

#[cfg(unix)]
pub(in crate::commands::work) fn record(
    session: &MutationSession,
    binding: Binding<'_>,
    recovery: Recovery,
    reason: String,
    plan: MutationPlan,
) -> miette::Result<()> {
    let root = session.root();
    let task = binding.task;
    if task.kind != TaskKind::ExtractConcepts || reason.trim().is_empty() {
        return Err(miette::miette!(
            "discovery recovery requires extraction and an explicit reason"
        ));
    }
    let recovery = recovery.validate()?;
    let _claim = match lk_queue::claim_queue(&root.join(".lorekeeper/queue")).into_diagnostic()? {
        lk_queue::QueueClaim::Held(claim) => claim,
        lk_queue::QueueClaim::Busy => return Err(miette::miette!("queue is busy")),
        lk_queue::QueueClaim::Unavailable(error) => {
            return Err(miette::miette!("queue claim unavailable: {error}"));
        }
    };
    let plan = plan_record(root, binding, recovery, reason, plan)?;
    session.apply(plan).into_diagnostic()
}

#[cfg(unix)]
fn plan_record(
    root: &Path,
    binding: Binding<'_>,
    recovery: Recovery,
    reason: String,
    mut plan: MutationPlan,
) -> miette::Result<MutationPlan> {
    let task = binding.task;
    crate::commands::queue::guard_current_task(root, task, &mut plan)?;
    let Subject::Queue {
        target_revision, ..
    } = &binding.prepared.subject
    else {
        return Err(miette::miette!(
            "discovery recovery requires a queue subject"
        ));
    };
    let target_path = Path::new(&task.target.vault_path);
    let target = std::fs::read_to_string(lk_vault::page_path(root, target_path).into_diagnostic()?)
        .into_diagnostic()?;
    if lk_core::concept::file_revision(&target) != *target_revision {
        return Err(miette::miette!("discovery recovery target changed"));
    }
    plan.require_unchanged(target_path, target);
    let old_queries: Vec<DiscoveryQuery> =
        serde_json::from_value(binding.prepared.context["purpose"]["discovery"]["queries"].clone())
            .into_diagnostic()?;
    let recipe_path = super::path(&task.task_id);
    let recipe_before = super::super::state::read_optional(root, &recipe_path)?;
    let current_queries = if let Some(raw) = &recipe_before {
        let record: super::DiscoveryRecord = serde_json::from_str(raw).into_diagnostic()?;
        if record.task_id != task.task_id || record.cache_hash != task.cache_hash {
            return Err(miette::miette!("discovery recipe belongs to another task"));
        }
        record
            .current_queries(task, target_revision)?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if current_queries != old_queries {
        return Err(miette::miette!("discovery recipe changed before recovery"));
    }
    let receipt_path = path(&task.task_id);
    let before = read(root, task)?;
    let (raw, mut history) = match before {
        Some((raw, history)) => (Some(raw), history),
        None => (
            None,
            History {
                schema: SCHEMA.into(),
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                receipts: Vec::new(),
            },
        ),
    };
    let request_revision = revision(binding.request)?;
    if history
        .receipts
        .iter()
        .any(|r| r.request_revision == request_revision)
    {
        return Err(miette::miette!(
            "discovery recovery request already recorded"
        ));
    }
    for (path, content) in &binding.prepared.pages {
        if *path != recipe_path {
            plan.require_unchanged(path, content.clone());
        } else if recipe_before.as_ref() != Some(content) {
            return Err(miette::miette!("captured discovery recipe changed"));
        }
    }
    let changed = old_queries != recovery.queries;
    let mut receipt = Receipt {
        receipt_revision: String::new(),
        request_revision,
        context_revision: binding.context_revision.into(),
        work_id: binding.work_id.into(),
        target_revision: target_revision.clone(),
        recipe_revision: revision(&recovery.queries)?,
        queries: recovery.queries.clone(),
        reason,
        disposition: if changed {
            Disposition::Changed
        } else {
            Disposition::Unchanged
        },
    };
    receipt.receipt_revision = receipt.revision(&history.task_id, &history.cache_hash)?;
    history.receipts.push(receipt);
    let after = serde_json::to_string(&history).into_diagnostic()?;
    History::decode(&after)?;
    if changed {
        let recipe = super::DiscoveryRecord {
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target_revision: target_revision.clone(),
            queries: recovery.queries,
        };
        plan.publish_expected(
            root,
            recipe_path,
            recipe_before,
            serde_json::to_string_pretty(&recipe).into_diagnostic()?,
        )
        .into_diagnostic()?;
    } else if let Some(before) = recipe_before {
        plan.require_unchanged(recipe_path, before);
    } else {
        plan.require_absent(recipe_path);
    }
    plan.publish_expected(root, receipt_path, raw, after)
        .into_diagnostic()?;
    Ok(plan)
}

pub(super) fn retire(root: &Path, task: &QueueTask, plan: &mut MutationPlan) -> miette::Result<()> {
    if let Some((raw, _)) = read(root, task)? {
        plan.remove(path(&task.task_id), raw);
    }
    Ok(())
}

pub(super) fn prune_entry(
    root: &Path,
    entry: &std::fs::DirEntry,
    pending: &BTreeSet<(String, String)>,
    plan: &mut MutationPlan,
) -> miette::Result<bool> {
    if !entry.file_type().into_diagnostic()?.is_file() {
        return Err(miette::miette!(
            "discovery recovery history is not a regular file"
        ));
    }
    let raw = std::fs::read_to_string(entry.path()).into_diagnostic()?;
    let history = History::decode(&raw)?;
    if entry.path() != root.join(path(&history.task_id)) {
        return Err(miette::miette!(
            "discovery recovery path differs from task identity"
        ));
    }
    if pending.contains(&(history.task_id.clone(), history.cache_hash.clone())) {
        return Ok(false);
    }
    plan.remove(path(&history.task_id), raw);
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::commands::work::{limits, resources, state::Work};
    use serde_json::json;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        config: lk_core::config::Config,
        task: QueueTask,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
            std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
            let config_path = root.join("config.yaml");
            std::fs::write(&config_path, "vault: {root: ., locale: en}\nidentity: {name: Test, email: test@example.com}\nsources:\n  notes: {type: manual, params: {inbox_dir: inbox}, concept_policy: supported-new}\n").unwrap();
            let config = lk_core::config::Config::load(&config_path).unwrap();
            let client = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue"));
            let task = client.build_task("fixture", TaskKind::ExtractConcepts,
                serde_json::from_value::<lk_queue::ExtractConceptsRequest>(json!({
                    "text":"Source observation.","source_id":"notes","source_type":"manual","date":"2026-10-24","locale":"en","concept_policy":"supported-new","focus":null,"categories":[],
                    "target":{"vault_path":"wiki/documents/source.md","kind":"document-concepts","anchor":"## Related Concepts"}
                })).unwrap());
            std::fs::write(root.join(&task.target.vault_path), format!("---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Content\nSource observation.\n\n## Related Concepts\n",task.cache_hash)).unwrap();
            std::fs::create_dir_all(root.join(".lorekeeper/queue")).unwrap();
            lk_queue::write_tasks_atomic(
                &root.join(".lorekeeper/queue/fixture.jsonl"),
                std::slice::from_ref(&task),
            )
            .unwrap();
            Self {
                _directory: directory,
                root,
                config,
                task,
            }
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn prepared(&self) -> Prepared {
            resources::task(self.root(), &self.config, &self.task.task_id).unwrap()
        }

        fn plan(&self, prepared: &Prepared, request: &Value) -> MutationPlan {
            plan_record(
                self.root(),
                Binding {
                    task: &self.task,
                    prepared,
                    request,
                    context_revision: &prepared.revision("config").unwrap(),
                    work_id: &"a".repeat(64),
                },
                Recovery {
                    queries: vec![DiscoveryQuery {
                        query: "Explicit owner".into(),
                        phrase: true,
                        spacing: false,
                    }],
                },
                "Exact owner discovery required.".into(),
                MutationPlan::default(),
            )
            .unwrap()
        }
    }

    #[test]
    fn recovery_preserves_exact_current_source_work_and_paid_observations() {
        let fixture = Fixture::new();
        let root = fixture.root();
        let session = MutationSession::acquire(root).unwrap();
        let source = resources::source_task(root, &fixture.config, &fixture.task.task_id).unwrap();
        let subject = source.revision("config").unwrap();
        let work = Work::create(
            &session,
            &source.resources,
            &subject,
            source.context.clone(),
            limits(&fixture.config).unwrap(),
        )
        .unwrap();
        loop {
            let progress = work.progress(root, &source.resources).unwrap();
            if progress.complete {
                break;
            }
            for node in progress.ready {
                let request = work.input(root, &node, &source.resources).unwrap();
                let output = serde_json::from_value(json!({"work_id":work.plan.id,"node_id":node,"input_revision":request["input_revision"],"summary":"Source report only; deployment unverified.","findings":[],"source_questions":[],"unresolved":[],"resolutions":[]})).unwrap();
                work.record(&session, output, &source.resources).unwrap();
            }
        }
        let before = work.result(root, &source.resources).unwrap();
        let plan_bytes = std::fs::read(
            root.join(".lorekeeper/work")
                .join(&work.plan.id)
                .join("plan.json"),
        )
        .unwrap();
        let prepared = fixture.prepared();
        session
            .apply(fixture.plan(&prepared, &json!({"actual_request":"before"})))
            .unwrap();
        let refreshed =
            resources::source_task(root, &fixture.config, &fixture.task.task_id).unwrap();
        assert_eq!(
            source.revision("config").unwrap(),
            refreshed.revision("config").unwrap()
        );
        let same = Work::create(
            &session,
            &refreshed.resources,
            &subject,
            refreshed.context.clone(),
            limits(&fixture.config).unwrap(),
        )
        .unwrap();
        assert_eq!(work.plan.id, same.plan.id);
        assert_eq!(before, same.result(root, &refreshed.resources).unwrap());
        assert_eq!(
            plan_bytes,
            std::fs::read(
                root.join(".lorekeeper/work")
                    .join(&same.plan.id)
                    .join("plan.json")
            )
            .unwrap()
        );
        assert_eq!(
            super::super::current(root, &fixture.task).unwrap().unwrap()[0].query,
            "Explicit owner"
        );
        assert_ne!(
            prepared.revision("config").unwrap(),
            fixture.prepared().revision("config").unwrap()
        );
    }

    #[test]
    fn interrupted_recipe_and_receipt_replay_rechecks_guards_at_each_prefix() {
        for receipt_written in [false, true] {
            for checkpointed in [false, true] {
                let fixture = Fixture::new();
                let root = fixture.root();
                let session = MutationSession::acquire(root).unwrap();
                let plan = fixture.plan(&fixture.prepared(), &json!({"request":"actual"}));
                let encoded =
                    |value: &Value| format!("{}\n", serde_json::to_string(value).unwrap());
                let revision =
                    lk_core::concept::file_revision(&serde_json::to_string(&plan).unwrap());
                let value = serde_json::to_value(plan).unwrap();
                let steps = value["steps"].as_array().unwrap();
                let applied = steps.len() - usize::from(!receipt_written);
                for step in &steps[..applied] {
                    if let Some(after) = step["after"].as_str() {
                        let destination = root.join(step["path"].as_str().unwrap());
                        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
                        std::fs::write(destination, after).unwrap();
                    }
                }
                drop(session);
                let journal = root.join(".lorekeeper/mutation");
                std::fs::create_dir_all(&journal).unwrap();
                let pending = encoded(&json!({"version":1,"plan":value}));
                std::fs::write(journal.join("pending.json"), &pending).unwrap();
                if checkpointed {
                    std::fs::write(journal.join("progress.json"),encoded(&json!({"plan_revision":revision,"completed":applied,"checksum":lk_core::concept::file_revision(&format!("{revision}:{applied}"))}))).unwrap();
                }
                let dependency = root.join(&fixture.task.target.vault_path);
                let original = std::fs::read(&dependency).unwrap();
                std::fs::write(&dependency, "Changed dependency").unwrap();
                assert!(MutationSession::acquire(root).is_err());
                assert_eq!(
                    std::fs::read_to_string(journal.join("pending.json")).unwrap(),
                    pending
                );
                std::fs::write(dependency, original).unwrap();
                let _recovered = MutationSession::acquire(root).unwrap();
                assert!(!journal.join("pending.json").exists());
                assert!(
                    blocked(root, &fixture.task, &json!({"request":"actual"}))
                        .unwrap()
                        .is_some()
                );
                assert!(
                    super::super::current(root, &fixture.task)
                        .unwrap()
                        .is_some()
                );
            }
        }
    }

    #[test]
    fn recovery_history_cleanup_preserves_live_references_and_refuses_corruption() {
        let fixture = Fixture::new();
        let root = fixture.root();
        let session = MutationSession::acquire(root).unwrap();
        let request = json!({"actual_request":"before"});
        session
            .apply(fixture.plan(&fixture.prepared(), &request))
            .unwrap();
        let pending = [(
            fixture.task.task_id.clone(),
            fixture.task.cache_hash.clone(),
        )]
        .into_iter()
        .collect();
        assert_eq!(super::super::prune(&session, &pending).unwrap(), 0);
        assert!(blocked(root, &fixture.task, &request).unwrap().is_some());
        assert!(
            blocked(root, &fixture.task, &json!({"actual_request":"changed"}))
                .unwrap()
                .is_none()
        );
        let recipe = root.join(super::super::path(&fixture.task.task_id));
        let saved_recipe = std::fs::read(&recipe).unwrap();
        std::fs::remove_file(&recipe).unwrap();
        assert!(blocked(root, &fixture.task, &request).is_err());
        std::fs::write(&recipe, saved_recipe).unwrap();
        let file = root.join(path(&fixture.task.task_id));
        let original = std::fs::read(&file).unwrap();
        let mut changed: Value = serde_json::from_slice(&original).unwrap();
        changed["receipts"][0]["request_revision"] = json!("b".repeat(64));
        std::fs::write(&file, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(blocked(root, &fixture.task, &request).is_err());
        std::fs::write(&file, "{corrupt").unwrap();
        assert!(super::super::prune(&session, &BTreeSet::new()).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{corrupt");
        std::fs::write(&file, original).unwrap();
        assert_eq!(super::super::prune(&session, &BTreeSet::new()).unwrap(), 2);
        assert!(!file.exists());
    }
}
