use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{GlobalOptions, find_config, load_config_with_revision};

mod discovery;
mod resources;
mod selection;
pub(crate) mod state;

use resources::{Prepared, Subject};
use state::{Work, WorkLimits, WorkOutput};

#[derive(clap::Subcommand)]
pub enum WorkCommand {
    /// Plan or resume one bounded candidate-screening packet without publishing knowledge
    Select {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
        /// Reread this exact packet without creating or recovering state
        #[arg(long)]
        batch_id: Option<String>,
        /// Receipt pagination limit (1..100); does not reduce candidate coverage
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Skip this many recorded receipt summaries; candidate coverage is unchanged
        #[arg(long, default_value = "0")]
        offset: usize,
    },
    /// Record exact decisions for a candidate-screening packet
    SelectRecord {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
        /// JSON decisions for every candidate in the exact selected packet
        #[arg(long)]
        file: PathBuf,
        /// Exact receipt revision being reassessed; ordinary retries never overwrite decisions
        #[arg(long)]
        previous_revision: Option<String>,
    },
    /// Plan complete retained-source observations for an oversized screening context
    SourcePlan {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
    },
    /// Record extraction discovery recipes against the exact target revision read
    Discover {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
        /// JSON {"target_revision":"...","queries":[...]}
        #[arg(long)]
        file: PathBuf,
    },
    /// Capture the exact current queue read scope before drafting, without writing
    Context {
        /// Exact current queue task identifier
        #[arg(long)]
        task_id: String,
        /// JSON {"queries":[...]} selected before concept extraction
        #[arg(long)]
        discovery_file: Option<PathBuf>,
    },
    /// Plan bounded evidence inspections without changing knowledge pages
    Plan {
        /// Exact current queue task identifier; mutually exclusive with --path
        #[arg(long, required_unless_present = "path", conflicts_with = "path")]
        task_id: Option<String>,
        /// Exact vault-relative concept or document address to audit
        #[arg(long, required_unless_present = "task_id")]
        path: Option<String>,
        /// Extraction discovery JSON captured before planning its complete read scope
        #[arg(long, requires = "task_id")]
        discovery_file: Option<PathBuf>,
    },
    /// Read one revision-pinned inspection or reduction request
    Read {
        /// Work identifier returned by plan
        work_id: String,
        /// Ready node identifier returned by status
        node_id: String,
    },
    /// Read the bounded completed root observation for final publication
    Result {
        /// Work identifier whose complete root observation is required
        work_id: String,
    },
    /// Record an observation; completion is structural coverage, not verified truth
    Record {
        /// JSON observation with exact work, node and input revision
        #[arg(long)]
        file: PathBuf,
    },
    /// Inspect resumable progress and the next bounded set of ready nodes
    Status {
        /// Work identifier returned by plan
        work_id: String,
        /// Maximum ready node identifiers to return
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Ready-node pagination offset
        #[arg(long, default_value = "0")]
        offset: usize,
    },
    /// Reuse an attributed observation only for identical evidence and purpose
    Reuse {
        /// Current work identifier receiving the attributed observation
        work_id: String,
        /// Current inspection node identifier
        node_id: String,
        /// Prior work identifier containing identical assigned evidence and purpose
        #[arg(long)]
        from_work: String,
        /// Prior inspection node identifier to reuse
        #[arg(long)]
        from_node: String,
    },
    /// Seal a complete reduction and stage its normal typed final result
    Finish {
        /// Complete work identifier whose root observation produced the result
        work_id: String,
        /// Normal typed queue result or single-subject audit assessment JSON
        #[arg(long)]
        file: PathBuf,
    },
}

pub(crate) fn limits(config: &lk_core::config::Config) -> miette::Result<WorkLimits> {
    let work = config.execution.effective_work().into_diagnostic()?;
    Ok(WorkLimits {
        max_input_bytes: work.max_input_bytes,
        max_result_bytes: work.max_result_bytes,
    })
}

fn expected(
    root: &Path,
    prepared: &Prepared,
    config: &lk_core::config::Config,
    config_revision: &str,
) -> miette::Result<Work> {
    Work::build(
        &prepared.resources,
        &prepared.revision(config_revision)?,
        prepared.context.clone(),
        limits(config)?,
    )?
    .with_saved_policy(root)
}

fn guard_prepared(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    prepared: Prepared,
    result: Value,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<bool> {
    let expected = expected(root, &prepared, config, config_revision)?;
    let manifest = root
        .join(".lorekeeper/work")
        .join(&expected.plan.id)
        .join("plan.json");
    match std::fs::symlink_metadata(&manifest) {
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && expected.plan.parts.len() == 1 =>
        {
            return Ok(false);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(miette::miette!(
                "bounded evidence work is required before publication; plan work for this exact subject"
            ));
        }
        Err(error) => return Err(miette::miette!("inspect bounded work: {error}")),
        Ok(_) => {}
    }
    let work = Work::load(root, &expected.plan.id)?;
    if work != expected {
        return Err(miette::miette!(
            "bounded work manifest differs from current inputs"
        ));
    }
    work.guard_completion(mutation, root, &prepared.resources, &result)?;
    work.plan_publication(mutation, root, &result)?;
    prepared.guard_pages(mutation);
    Ok(true)
}

pub(super) struct QueuePublicationGuard {
    work: lk_vault::MutationPlan,
    pages: std::collections::BTreeMap<PathBuf, String>,
    owned_pages: std::collections::BTreeSet<PathBuf>,
    bounded: bool,
}

impl QueuePublicationGuard {
    pub(super) fn append_to(
        self,
        mutation: &mut lk_vault::MutationPlan,
        owned: &std::collections::BTreeMap<PathBuf, String>,
    ) -> miette::Result<bool> {
        for (path, captured) in self.pages {
            if let Some(original) = owned.get(&path) {
                if original != &captured {
                    return Err(miette::miette!(
                        "publication input differs from captured read context: {}",
                        path.display()
                    ));
                }
            } else {
                mutation.require_unchanged(path, captured);
            }
        }
        mutation.extend(self.work);
        Ok(self.bounded)
    }
}

pub(super) fn guard_queue(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    result: Value,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<bool> {
    let guard = prepare_queue_publication(root, config, config_revision, task_id, result)?;
    let owned = guard
        .pages
        .iter()
        .filter(|(path, _)| guard.owned_pages.contains(*path))
        .map(|(path, raw)| (path.clone(), raw.clone()))
        .collect();
    guard.append_to(mutation, &owned)
}

pub(super) fn prepare_queue_publication(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    result: Value,
) -> miette::Result<QueuePublicationGuard> {
    let root = root.canonicalize().into_diagnostic()?;
    let mut prepared = resources::task_for_publication(&root, config, task_id)?;
    let mut selection_guards = lk_vault::MutationPlan::default();
    let mut owned_pages = std::collections::BTreeSet::new();
    let scoped_extraction = prepared.context["purpose"]["task_kind"]
        == serde_json::to_value(lk_queue::TaskKind::ExtractConcepts).into_diagnostic()?;
    if scoped_extraction {
        let typed: lk_queue::TaskResult =
            serde_json::from_value(result.clone()).into_diagnostic()?;
        if !typed.concepts.is_empty() && typed.discovery.is_empty() {
            return Err(miette::miette!(
                "nonempty concept extraction requires discovery before drafting"
            ));
        }
        let selected_owners = if let Some(selection) = &typed.selection {
            let (selected, owners) = selected_task(
                &root,
                config,
                config_revision,
                task_id,
                &typed.discovery,
                selection,
                &mut selection_guards,
            )?;
            prepared = selected;
            Some(owners)
        } else {
            prepared = resources::with_discovery(&root, config, prepared, &typed.discovery)?;
            None
        };
        for concept in &typed.concepts {
            if let lk_queue::ConceptAdmission::Reuse { owner } = &concept.admission {
                let canonical = lk_vault::page_path(&root, Path::new(&owner.path))
                    .into_diagnostic()?
                    .canonicalize()
                    .into_diagnostic()?;
                let address = canonical
                    .strip_prefix(&root)
                    .into_diagnostic()?
                    .to_string_lossy();
                if selected_owners
                    .as_ref()
                    .is_some_and(|owners| !owners.contains(address.as_ref()))
                    || !prepared.resources.iter().any(|resource| {
                        (selected_owners.is_some()
                            || resource.authority == lk_core::semantic::SemanticAuthority::Context)
                            && resource.address == address
                            && resource.revision == owner.revision
                    })
                {
                    return Err(miette::miette!(
                        "selected concept owner is outside the captured discovery context or its read revision changed"
                    ));
                }
                owned_pages.insert(
                    canonical
                        .strip_prefix(&root)
                        .into_diagnostic()?
                        .to_path_buf(),
                );
            }
        }
    }
    let scoped_synthesis = prepared.context["purpose"]["task_kind"]
        == serde_json::to_value(lk_queue::TaskKind::SynthesizeConcept).into_diagnostic()?;
    if scoped_synthesis || scoped_extraction {
        let revision = prepared.revision(config_revision)?;
        if result.get("context_revision").and_then(Value::as_str) != Some(revision.as_str()) {
            return Err(miette::miette!(
                "semantic read scope changed or was not captured before drafting; reread current context and regenerate the result"
            ));
        }
    }
    if scoped_extraction && !prepared.context["purpose"]["source_observations"].is_null() {
        let typed: lk_queue::TaskResult =
            serde_json::from_value(result.clone()).into_diagnostic()?;
        let source = resources::source_task(&root, config, task_id)?;
        let source_work = expected(&root, &source, config, config_revision)?;
        if prepared.context["purpose"]["source_observations"]["work_id"] != source_work.plan.id {
            return Err(miette::miette!(
                "retained source observations differ from their current work identity"
            ));
        }
        source_work.plan_consumption(&mut selection_guards, &root, &source.resources, &typed)?;
    }
    if let Subject::Queue { target, .. } = &prepared.subject {
        let target = lk_vault::page_path(&root, Path::new(target))
            .into_diagnostic()?
            .canonicalize()
            .into_diagnostic()?;
        owned_pages.insert(target.strip_prefix(&root).into_diagnostic()?.to_path_buf());
    }
    let pages = std::mem::take(&mut prepared.pages);
    let mut work = selection_guards;
    let bounded = guard_prepared(&root, config, config_revision, prepared, result, &mut work)?;
    Ok(QueuePublicationGuard {
        work,
        pages,
        owned_pages,
        bounded,
    })
}

pub(super) fn task_context(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    discovery: Option<&[lk_core::discovery::DiscoveryQuery]>,
    discovery_file: Option<&Path>,
) -> miette::Result<Value> {
    let root = root.canonicalize().into_diagnostic()?;
    let task = super::queue::select_task(
        &root,
        task_id,
        super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    let prepared = if task.kind == lk_queue::TaskKind::ExtractConcepts && is_progressive(config)? {
        let retained = progressive_discovery(&root, &task, discovery)?;
        let Some(queries) = retained.as_deref() else {
            return Ok(json!({"status":"needs-discovery", "context_revision":null,
                "requires_screening":true,"selection":null,"resources":null,
                "authority":"Record explicit discovery queries against the exact target before selecting candidate evidence. No semantic read or completion is certified."}));
        };
        match screening(&root, config, config_revision, task_id, queries)? {
            ScreeningPreparation::Source { work, .. } => {
                return Ok(
                    json!({"status":"needs-source-observations","context_revision":null,
                    "requires_screening":true,"requires_source_work":true,"selection":null,"resources":null,
                    "part_count":work.plan.parts.len(),
                    "plan_command":["lore","work","source-plan","--task-id",task_id]}),
                );
            }
            ScreeningPreparation::Candidates(screening) => {
                let planned = screening.selection.is_planned(&root)?;
                let ledger = planned
                    .then(|| screening.selection.ledger(&root))
                    .transpose()?;
                if let Some(ledger) = ledger.filter(|ledger| ledger.progress.complete) {
                    admitted_task(&screening, &ledger.binding()?, &ledger.selected_owners)?
                } else {
                    return Ok(json!({"status":"needs-screening","context_revision":null,
                        "requires_screening":true,"requires_source_work":false,"resources":null,
                        "candidate_count":screening.inventory.capture.candidates.len(),
                        "selection_id":screening.selection.id,"planned":planned,
                        "plan_command":["lore","work","select","--task-id",task_id],
                        "authority":"Complete retrieval is not a semantic assessment. Read and record each assigned candidate-card decision; unresolved applicability remains deferred. Selected bodies still require full evidence inspection before publication."}));
                }
            }
        }
    } else {
        match discovery {
            Some(queries) => resources::task_with_discovery(&root, config, task_id, queries)?,
            None => resources::task(&root, config, task_id)?,
        }
    };
    let work = expected(&root, &prepared, config, config_revision)?;
    let requires_bounded_work = work.plan.parts.len() > 1;
    let plan_command = if requires_bounded_work {
        let mut command = vec![
            json!("lore"),
            json!("work"),
            json!("plan"),
            json!("--task-id"),
            json!(task_id),
        ];
        if let Some(path) = discovery_file {
            command.push(json!("--discovery-file"));
            command.push(
                serde_json::to_value(path.canonicalize().into_diagnostic()?).into_diagnostic()?,
            );
        }
        Value::Array(command)
    } else {
        Value::Null
    };
    let resources = if requires_bounded_work {
        Value::Null
    } else {
        json!(prepared.resources.iter().map(|resource| json!({
            "address":resource.address,"revision":resource.revision,"authority":resource.authority,
        })).collect::<Vec<_>>())
    };
    Ok(json!({
        "status":"ready",
        "requires_screening":false,
        "selection":prepared.context["purpose"]["extraction_scope"]["selection"],
        "context_revision": prepared.revision(config_revision)?,
        "subject": prepared.subject,
        "context": prepared.context,
        "resource_count":prepared.resources.len(),
        "part_count":work.plan.parts.len(),
        "requires_bounded_work":requires_bounded_work,
        "resources":resources,
        "plan_command":plan_command,
        "authority":"This binds the selected read scope, not a read receipt or semantic verification. Capture it before drafting; changed scope requires rereading and regenerating the result. Multi-part resources are not listed here: execute plan_command and complete the exact bounded inspections before publication.",
    }))
}

pub(super) fn has_current_discovery(
    root: &Path,
    task: &lk_queue::QueueTask,
) -> miette::Result<bool> {
    discovery::current(root, task).map(|queries| queries.is_some())
}

pub(super) fn retire_discovery(
    root: &Path,
    task: &lk_queue::QueueTask,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<()> {
    discovery::retire(root, task, plan)
}

pub(super) fn prune_discovery(
    session: &lk_vault::MutationSession,
    pending: &std::collections::BTreeSet<(String, String)>,
) -> miette::Result<usize> {
    discovery::prune(session, pending)
}

pub(super) fn prune_selection(
    session: &lk_vault::MutationSession,
    pending: &std::collections::BTreeSet<(String, String)>,
) -> miette::Result<usize> {
    selection::prune(session, pending)
}

pub(super) fn needs_discovery(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
) -> miette::Result<bool> {
    if task.kind != lk_queue::TaskKind::ExtractConcepts || has_current_discovery(root, task)? {
        return Ok(false);
    }
    let prepared = resources::task(root, config, &task.task_id)?;
    Ok(expected(root, &prepared, config, config_revision)?
        .plan
        .parts
        .len()
        > 1)
}

fn is_progressive(config: &lk_core::config::Config) -> miette::Result<bool> {
    Ok(config
        .execution
        .effective_work()
        .into_diagnostic()?
        .extraction_context
        == lk_core::execution::ExtractionContext::Progressive)
}

fn progressive_discovery(
    root: &Path,
    task: &lk_queue::QueueTask,
    supplied: Option<&[lk_core::discovery::DiscoveryQuery]>,
) -> miette::Result<Option<Vec<lk_core::discovery::DiscoveryQuery>>> {
    let retained = discovery::current(root, task)?;
    if let Some(supplied) = supplied {
        let supplied = discovery::validate_queries(supplied)?;
        let current = retained.as_ref().ok_or_else(|| {
            miette::miette!(
                "progressive --discovery-file requires current recorded recipes; run lore work discover before candidate examination"
            )
        })?;
        if &supplied != current {
            return Err(miette::miette!(
                "--discovery-file differs from current recorded recipes; record the intended queries with lore work discover before progressive examination"
            ));
        }
    }
    Ok(retained)
}

fn task_for_plan(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    discovery: Option<&[lk_core::discovery::DiscoveryQuery]>,
) -> miette::Result<Prepared> {
    let task = super::queue::select_task(
        root,
        task_id,
        super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    if task.kind != lk_queue::TaskKind::ExtractConcepts || !is_progressive(config)? {
        return match discovery {
            Some(queries) => resources::task_with_discovery(root, config, task_id, queries),
            None => resources::task(root, config, task_id),
        };
    }
    let queries = progressive_discovery(root, &task, discovery)?.ok_or_else(|| {
        miette::miette!(
            "progressive extraction requires recorded discovery before planning; run lore work discover and inspect lore work context"
        )
    })?;
    let ScreeningPreparation::Candidates(screening) =
        screening(root, config, config_revision, task_id, &queries)?
    else {
        return Err(miette::miette!(
            "source observations are incomplete; use lore work source-plan before planning selected evidence"
        ));
    };
    if !screening.selection.is_planned(root)? {
        return Err(miette::miette!(
            "candidate screening is incomplete; use lore work select before planning selected evidence"
        ));
    }
    let ledger = screening.selection.ledger(root)?;
    admitted_task(&screening, &ledger.binding()?, &ledger.selected_owners)
}

struct Screening {
    selection: selection::Selection,
    inventory: resources::SelectionInventory,
}

enum ScreeningPreparation {
    Source {
        work: Box<Work>,
        prepared: Box<Prepared>,
    },
    Candidates(Box<Screening>),
}

fn screening(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
) -> miette::Result<ScreeningPreparation> {
    let mut inventory = resources::selection_inventory(root, config, task_id, queries)?;
    let Subject::Queue {
        task_id,
        cache_hash,
        target,
        target_revision,
    } = &inventory.source.subject
    else {
        return Err(miette::miette!(
            "candidate screening requires a queue subject"
        ));
    };
    let binding = selection::SelectionBinding {
        task_id: task_id.clone(),
        cache_hash: cache_hash.clone(),
        target: target.clone(),
        target_revision: target_revision.clone(),
        config_revision: config_revision.into(),
        inventory_revision: inventory.revision.clone(),
    };
    let bounds = limits(config)?;
    let source_revision = inventory.source.revision(config_revision)?;
    let mut context = if inventory.capture.candidates.is_empty() {
        json!({"role":"no-candidates", "source_revision":source_revision})
    } else {
        json!({"role":"complete-retained-input", "source_revision":source_revision,
            "context":inventory.source.context, "resources":inventory.source.resources})
    };
    if !selection::Selection::source_fits(
        &binding,
        &context,
        &inventory.capture,
        bounds.max_input_bytes,
        bounds.max_result_bytes,
    )? {
        let work = expected(root, &inventory.source, config, config_revision)?;
        if !work.is_planned(root)? || !work.progress(root, &inventory.source.resources)?.complete {
            return Ok(ScreeningPreparation::Source {
                work: Box::new(work),
                prepared: Box::new(inventory.source),
            });
        }
        let (result, guards) = work.result_with_guards(root, &inventory.source.resources)?;
        if result["output"]["unresolved"]
            .as_array()
            .is_some_and(|values| !values.is_empty())
        {
            return Err(miette::miette!(
                "source observations retain processing blockers; resolve or replan before candidate screening"
            ));
        }
        inventory.pages.extend(guards);
        context = json!({"role":"complete-bounded-source-observations", "source_revision":source_revision,
            "observation":result});
    }
    let selection = selection::Selection::build(
        binding,
        context,
        &inventory.capture,
        bounds.max_input_bytes,
        bounds.max_result_bytes,
    )?;
    Ok(ScreeningPreparation::Candidates(Box::new(Screening {
        selection,
        inventory,
    })))
}

fn current_screening(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<ScreeningPreparation> {
    let task = super::queue::select_task(
        root,
        task_id,
        super::queue::TaskSelection::CurrentOrDoneExtraction,
    )?;
    let queries = discovery::current(root, &task)?.ok_or_else(|| {
        miette::miette!(
            "candidate screening requires current explicit discovery; record work discover first"
        )
    })?;
    screening(root, config, config_revision, task_id, &queries)
}

impl ScreeningPreparation {
    fn persist(
        &self,
        session: &lk_vault::MutationSession,
        config: &lk_core::config::Config,
        config_revision: &str,
    ) -> miette::Result<()> {
        match self {
            Self::Source { prepared, .. } => {
                Work::create(
                    session,
                    &prepared.resources,
                    &prepared.revision(config_revision)?,
                    prepared.context.clone(),
                    limits(config)?,
                )?;
            }
            Self::Candidates(screening) => screening
                .selection
                .create(session, &screening.inventory.pages)?,
        }
        Ok(())
    }
}

pub(super) struct ScreeningTurn {
    pub selection_id: String,
    pub request: Option<Value>,
    pub complete: bool,
    pub unknown: usize,
    pub deferred: usize,
    pub completed_batches: usize,
    pub batches: usize,
}

pub(super) fn prepare_screening(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<ScreeningTurn> {
    let root = root.canonicalize().into_diagnostic()?;
    let prepared = current_screening(&root, config, config_revision, task_id)?;
    let ScreeningPreparation::Candidates(screening) = prepared else {
        return Err(miette::miette!(
            "source observations are incomplete; resume source work before candidate screening"
        ));
    };
    let (progress, request) = screening.selection.prepare(&root)?;
    Ok(ScreeningTurn {
        selection_id: screening.selection.id,
        request,
        complete: progress.complete,
        unknown: progress.unknown,
        deferred: progress.deferred,
        completed_batches: progress.completed_batches,
        batches: progress.batches,
    })
}

pub(super) fn record_screening(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    output: Value,
    previous_revision: Option<&str>,
) -> miette::Result<usize> {
    let session = lk_vault::MutationSession::acquire(root).into_diagnostic()?;
    let prepared = current_screening(session.root(), config, config_revision, task_id)?;
    let ScreeningPreparation::Candidates(screening) = prepared else {
        return Err(miette::miette!(
            "candidate screening source scope changed; replan"
        ));
    };
    Ok(screening
        .selection
        .record(
            &session,
            &screening.inventory.pages,
            serde_json::from_value(output).into_diagnostic()?,
            previous_revision,
        )?
        .completed_batches)
}

fn selected_task(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
    binding: &lk_queue::ExtractionSelection,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<(Prepared, std::collections::BTreeSet<String>)> {
    let ScreeningPreparation::Candidates(screening) =
        screening(root, config, config_revision, task_id, queries)?
    else {
        return Err(miette::miette!(
            "source observations are incomplete; retained extraction cannot publish"
        ));
    };
    let ledger = screening.selection.guard(root, mutation, binding)?;
    Ok((
        admitted_task(&screening, binding, &ledger.selected_owners)?,
        ledger.selected_owners,
    ))
}

fn admitted_task(
    screening: &Screening,
    binding: &lk_queue::ExtractionSelection,
    selected: &std::collections::BTreeSet<String>,
) -> miette::Result<Prepared> {
    let mut prepared = resources::task_with_selection(&screening.inventory, binding, selected)?;
    if screening.selection.source_context()["role"] == "complete-bounded-source-observations" {
        prepared.context["purpose"]["source_observations"] =
            screening.selection.source_context()["observation"].clone();
    }
    Ok(prepared)
}

fn load_discovery(path: &Path) -> miette::Result<Vec<lk_core::discovery::DiscoveryQuery>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Discovery {
        queries: Vec<lk_core::discovery::DiscoveryQuery>,
    }
    let value: Discovery =
        serde_json::from_slice(&std::fs::read(path).into_diagnostic()?).into_diagnostic()?;
    Ok(value.queries)
}

pub(super) fn guard_audit(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    assessment: &lk_graph::audit::AuditAssessment,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<bool> {
    let root = root.canonicalize().into_diagnostic()?;
    guard_prepared(
        &root,
        config,
        config_revision,
        resources::audit(&root, config, &assessment.path)?,
        json!({"assessments":[assessment]}),
        mutation,
    )
}

fn load(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<(Work, Prepared)> {
    let work = Work::load(root, id)?;
    let prepared = resources::refresh(root, config, config_revision, &work.context)?;
    if expected(root, &prepared, config, config_revision)? != work {
        return Err(miette::miette!(
            "work inputs, configuration or bounded execution policy changed; replan"
        ));
    }
    Ok((work, prepared))
}

pub(super) enum TaskWork {
    WholeInput { bytes: usize },
    Partitioned { id: String },
    Source { id: String },
    Screening,
}

pub(super) fn prepare_task(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<TaskWork> {
    prepare_task_with_audit(root, config, config_revision, task_id, None)
}

pub(super) fn prepare_task_with_audit(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    audit: Option<&lk_graph::audit::AuditCandidate>,
) -> miette::Result<TaskWork> {
    let root = root.canonicalize().into_diagnostic()?;
    let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
    let task = super::queue::current_task(&root, task_id)?;
    let prepared = if task.kind == lk_queue::TaskKind::ExtractConcepts
        && is_progressive(config)?
        && discovery::current(&root, &task)?.is_some()
    {
        let selected = current_screening(&root, config, config_revision, task_id)?;
        selected.persist(&session, config, config_revision)?;
        match selected {
            ScreeningPreparation::Source { work, .. } => {
                return Ok(TaskWork::Source { id: work.plan.id });
            }
            ScreeningPreparation::Candidates(screening) => {
                let ledger = screening.selection.ledger(&root)?;
                if !ledger.progress.complete {
                    return Ok(TaskWork::Screening);
                }
                admitted_task(&screening, &ledger.binding()?, &ledger.selected_owners)?
            }
        }
    } else {
        resources::task_with_audit(&root, config, task_id, audit)?
    };
    let work = expected(&root, &prepared, config, config_revision)?;
    let stored = root
        .join(".lorekeeper/work")
        .join(&work.plan.id)
        .join("plan.json");
    let planned = match std::fs::symlink_metadata(stored) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(miette::miette!("inspect work plan: {error}")),
    };
    if work.plan.parts.len() == 1 && !planned {
        return Ok(TaskWork::WholeInput {
            bytes: work.whole_input_bytes(&prepared.resources)?,
        });
    }
    Work::create(
        &session,
        &prepared.resources,
        &prepared.revision(config_revision)?,
        prepared.context,
        limits(config)?,
    )?;
    Ok(TaskWork::Partitioned { id: work.plan.id })
}

pub(super) fn prepare_audit(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
) -> miette::Result<String> {
    let root = root.canonicalize().into_diagnostic()?;
    let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
    let prepared = resources::audit_candidate(&root, config, candidate)?;
    let work = Work::create(
        &session,
        &prepared.resources,
        &prepared.revision(config_revision)?,
        prepared.context,
        limits(config)?,
    )?;
    Ok(work.plan.id)
}

pub(super) fn audit_needs_work(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
) -> miette::Result<bool> {
    let root = root.canonicalize().into_diagnostic()?;
    let prepared = resources::audit_candidate(&root, config, candidate)?;
    Ok(expected(&root, &prepared, config, config_revision)?
        .plan
        .parts
        .len()
        > 1)
}

pub(super) fn prepare_inspection(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<(state::WorkProgress, Option<Value>)> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, prepared) = load(&root, config, config_revision, id)?;
    work.prepare_inspection(&root, &prepared.resources)
}

#[cfg(unix)]
pub(super) fn prepare_inspections(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    limit: usize,
) -> miette::Result<(state::WorkProgress, Vec<Value>)> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, prepared) = load(&root, config, config_revision, id)?;
    work.prepare_inspections(&root, &prepared.resources, limit)
}

pub(super) fn record_inspection(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    node: &str,
    output: WorkOutput,
) -> miette::Result<state::WorkProgress> {
    if output.work_id != id || output.node_id != node {
        return Err(miette::miette!(
            "observation does not answer the selected work node"
        ));
    }
    let session = lk_vault::MutationSession::acquire(root).into_diagnostic()?;
    let (work, prepared) = load(root, config, config_revision, id)?;
    work.record(&session, output, &prepared.resources)
}

pub(super) fn verify_finished(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<()> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, prepared) = load(&root, config, config_revision, id)?;
    let result = std::fs::read(root.join(".lorekeeper/work").join(id).join("final.json"))
        .into_diagnostic()?;
    let result: Value = serde_json::from_slice(&result).into_diagnostic()?;
    work.guard_completion(
        &mut lk_vault::MutationPlan::default(),
        &root,
        &prepared.resources,
        &result,
    )
}

pub(super) fn is_finished(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<bool> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, _) = load(&root, config, config_revision, id)?;
    let path = root
        .join(".lorekeeper/work")
        .join(&work.plan.id)
        .join("final.json");
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(miette::miette!("inspect final result: {error}")),
        Ok(_) => verify_finished(&root, config, config_revision, id).map(|()| true),
    }
}

fn queue_result_path(kind: lk_queue::TaskKind, result: &Value) -> miette::Result<PathBuf> {
    let suffix = match kind {
        lk_queue::TaskKind::ExtractConcepts => ".json",
        lk_queue::TaskKind::SynthesizeConcept => lk_queue::SYNTHESIS_RESULT_SUFFIX,
        _ => lk_queue::SECTION_RESULT_SUFFIX,
    };
    let digest = blake3::hash(&serde_json::to_vec(result).into_diagnostic()?)
        .to_hex()
        .to_string();
    Ok(PathBuf::from(".lorekeeper/queue/results").join(format!("{digest}{suffix}")))
}

pub(super) fn restage_finished(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<()> {
    let session = lk_vault::MutationSession::acquire(root).into_diagnostic()?;
    let (work, prepared) = load(root, config, config_revision, id)?;
    let final_path = PathBuf::from(".lorekeeper/work")
        .join(id)
        .join("final.json");
    let final_raw =
        std::fs::read_to_string(lk_vault::page_path(root, &final_path).into_diagnostic()?)
            .into_diagnostic()?;
    let result: Value = serde_json::from_str(&final_raw).into_diagnostic()?;
    let mut mutation = lk_vault::MutationPlan::default();
    work.guard_completion(&mut mutation, root, &prepared.resources, &result)?;
    prepared.guard_pages(&mut mutation);
    mutation.require_unchanged(final_path, final_raw);
    let path = match &prepared.subject {
        Subject::Queue { task_id, .. } => {
            queue_result_path(super::queue::current_task(root, task_id)?.kind, &result)?
        }
        Subject::Audit { .. } => PathBuf::from(".lorekeeper/work")
            .join(id)
            .join("assessment.json"),
    };
    match std::fs::read_to_string(lk_vault::page_path(root, &path).into_diagnostic()?) {
        Ok(raw) => {
            let existing: Value = serde_json::from_str(&raw).into_diagnostic()?;
            if existing != result {
                return Err(miette::miette!(
                    "staged result differs from the verified sealed result; retained for inspection"
                ));
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(miette::miette!("read staged result: {error}")),
    }
    mutation
        .publish_expected(
            root,
            path,
            None,
            serde_json::to_string(&result).into_diagnostic()?,
        )
        .into_diagnostic()?;
    session.apply(mutation).into_diagnostic()
}

fn progress(
    work: &Work,
    root: &Path,
    prepared: &Prepared,
    limit: usize,
    offset: usize,
) -> miette::Result<Value> {
    let status = work.progress(root, &prepared.resources)?;
    let total_ready = status.ready.len();
    let ready = status
        .ready
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let next_offset = (offset.saturating_add(ready.len()) < total_ready)
        .then_some(offset.saturating_add(ready.len()));
    Ok(json!({"work_id":work.plan.id,"subject":prepared.subject,
        "context_revision":work.plan.subject_revision,
        "resources":work.plan.resources.len(),"parts":work.plan.parts.len(),
        "completed":status.completed,"total":status.total,"complete":status.complete,
        "root_node":status.root_node,"ready_total":total_ready,"ready":ready,
        "limit":limit,"offset":offset,"next_offset":next_offset}))
}

pub async fn run(opts: &GlobalOptions, command: WorkCommand) -> miette::Result<()> {
    let (config, config_revision) = load_config_with_revision(&find_config(opts)?)?;
    let root = config.vault.root_path().canonicalize().into_diagnostic()?;
    let output = match command {
        WorkCommand::Select {
            task_id,
            batch_id,
            limit,
            offset,
        } => {
            if !(1..=100).contains(&limit) {
                return Err(miette::miette!("receipt limit must be in 1..100"));
            }
            let session = batch_id
                .is_none()
                .then(|| lk_vault::MutationSession::acquire(&root).into_diagnostic())
                .transpose()?;
            let prepared = current_screening(&root, &config, &config_revision, &task_id)?;
            if let Some(session) = &session {
                prepared.persist(session, &config, &config_revision)?;
            }
            match prepared {
                ScreeningPreparation::Source { work, prepared } => json!({
                    "status":"needs-source-observations",
                    "source_work":progress(&work, &root, &prepared, 20, 0)?,
                    "plan_command":["lore","work","source-plan","--task-id",task_id],
                }),
                ScreeningPreparation::Candidates(screening) => {
                    let (mut progress, mut request) = screening.selection.prepare(&root)?;
                    if let Some(batch_id) = &batch_id {
                        request = Some(screening.selection.packet(&root, batch_id)?);
                    }
                    let receipt = batch_id
                        .as_ref()
                        .and_then(|id| {
                            progress
                                .receipts
                                .iter()
                                .find(|receipt| &receipt.batch_id == id)
                        })
                        .map(serde_json::to_value)
                        .transpose()
                        .into_diagnostic()?;
                    let receipt_total = progress.receipts.len();
                    progress.receipts = progress
                        .receipts
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .collect();
                    let next = offset.saturating_add(progress.receipts.len());
                    let status = if progress.complete {
                        "ready"
                    } else if request.is_none() {
                        "deferred"
                    } else {
                        "screening"
                    };
                    json!({"status":status, "progress":progress,"request":request,"receipt":receipt,
                        "receipt_total":receipt_total,"receipt_limit":limit,"receipt_offset":offset,
                        "receipt_next_offset":(next<receipt_total).then_some(next)})
                }
            }
        }
        WorkCommand::SelectRecord {
            task_id,
            file,
            previous_revision,
        } => {
            let output = serde_json::from_slice(&std::fs::read(file).into_diagnostic()?)
                .into_diagnostic()?;
            let completed = record_screening(
                &root,
                &config,
                &config_revision,
                &task_id,
                output,
                previous_revision.as_deref(),
            )?;
            json!({"completed_batches":completed,"next_command":["lore","work","select","--task-id",task_id]})
        }
        WorkCommand::SourcePlan { task_id } => {
            let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
            let prepared = resources::source_task(&root, &config, &task_id)?;
            let work = Work::create(
                &session,
                &prepared.resources,
                &prepared.revision(&config_revision)?,
                prepared.context.clone(),
                limits(&config)?,
            )?;
            progress(&work, &root, &prepared, 20, 0)?
        }
        WorkCommand::Discover { task_id, file } => {
            let request = serde_json::from_slice(&std::fs::read(file).into_diagnostic()?)
                .into_diagnostic()?;
            let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
            discovery::record(&session, &task_id, request)?;
            json!({"recorded":true,"task_id":task_id})
        }
        WorkCommand::Context {
            task_id,
            discovery_file,
        } => {
            let discovery = discovery_file.as_deref().map(load_discovery).transpose()?;
            task_context(
                &root,
                &config,
                &config_revision,
                &task_id,
                discovery.as_deref(),
                discovery_file.as_deref(),
            )?
        }
        WorkCommand::Plan {
            task_id,
            path,
            discovery_file,
        } => {
            let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
            let discovery = discovery_file.as_deref().map(load_discovery).transpose()?;
            let prepared = match (task_id, path) {
                (Some(id), None) => {
                    task_for_plan(&root, &config, &config_revision, &id, discovery.as_deref())?
                }
                (None, Some(path)) => resources::audit(&root, &config, &path)?,
                _ => return Err(miette::miette!("select exactly one task or audit path")),
            };
            let work = Work::create(
                &session,
                &prepared.resources,
                &prepared.revision(&config_revision)?,
                prepared.context.clone(),
                limits(&config)?,
            )?;
            progress(&work, &root, &prepared, 20, 0)?
        }
        WorkCommand::Read { work_id, node_id } => {
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            work.input(&root, &node_id, &prepared.resources)?
        }
        WorkCommand::Result { work_id } => {
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            work.result(&root, &prepared.resources)?
        }
        WorkCommand::Status {
            work_id,
            limit,
            offset,
        } => {
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            progress(&work, &root, &prepared, limit, offset)?
        }
        WorkCommand::Record { file } => {
            let output: WorkOutput =
                serde_json::from_slice(&std::fs::read(file).into_diagnostic()?)
                    .into_diagnostic()?;
            let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
            let (work, prepared) = load(&root, &config, &config_revision, &output.work_id)?;
            work.record(&session, output, &prepared.resources)?;
            progress(&work, &root, &prepared, 20, 0)?
        }
        WorkCommand::Reuse {
            work_id,
            node_id,
            from_work,
            from_node,
        } => {
            let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            let prior = Work::load(&root, &from_work)?;
            work.reuse_from(&session, &prior, &from_node, &node_id, &prepared.resources)?;
            progress(&work, &root, &prepared, 20, 0)?
        }
        WorkCommand::Finish { work_id, file } => {
            let result: Value = serde_json::from_slice(&std::fs::read(file).into_diagnostic()?)
                .into_diagnostic()?;
            let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            finish(&session, &work, &prepared, result)?
        }
    };
    println!("{}", serde_json::to_string(&output).into_diagnostic()?);
    Ok(())
}

fn finish(
    session: &lk_vault::MutationSession,
    work: &Work,
    prepared: &Prepared,
    result: Value,
) -> miette::Result<Value> {
    let (path, mutation) = plan_finish(session, work, prepared, result)?;
    session.apply(mutation).into_diagnostic()?;
    Ok(json!({"work_id":work.plan.id,"staged_result":path,"canonical_published":false}))
}

fn plan_finish(
    session: &lk_vault::MutationSession,
    work: &Work,
    prepared: &Prepared,
    result: Value,
) -> miette::Result<(PathBuf, lk_vault::MutationPlan)> {
    if prepared.context["purpose"]["extraction_scope"]["policy"] == "source-proposals-v1" {
        return Err(miette::miette!(
            "source observations support candidate screening; they cannot publish a final knowledge result"
        ));
    }
    let result = match &prepared.subject {
        Subject::Queue { task_id, .. } => {
            match super::queue::current_task(session.root(), task_id)?.kind {
                lk_queue::TaskKind::ExtractConcepts => serde_json::to_value(
                    serde_json::from_value::<lk_queue::TaskResult>(result).into_diagnostic()?,
                )
                .into_diagnostic()?,
                lk_queue::TaskKind::SynthesizeConcept => serde_json::to_value(
                    serde_json::from_value::<lk_queue::SynthesisResult>(result)
                        .into_diagnostic()?,
                )
                .into_diagnostic()?,
                _ => serde_json::to_value(
                    serde_json::from_value::<lk_queue::SectionResult>(result).into_diagnostic()?,
                )
                .into_diagnostic()?,
            }
        }
        Subject::Audit { .. } => {
            let typed: lk_graph::audit::AuditResults =
                serde_json::from_value(result).into_diagnostic()?;
            let dependencies: Vec<String> =
                serde_json::from_value(prepared.context["purpose"]["review_dependencies"].clone())
                    .into_diagnostic()?;
            if typed
                .assessments
                .iter()
                .any(|assessment| assessment.reviews != dependencies)
            {
                return Err(miette::miette!(
                    "final assessment must list all exact current scoped review dependencies"
                ));
            }
            json!({"assessments":typed.assessments})
        }
    };
    if serde_json::to_vec(&result).into_diagnostic()?.len() > work.plan.max_result_bytes {
        return Err(miette::miette!(
            "final typed result exceeds the bounded result limit"
        ));
    }
    let completed = work.capture_completion(session.root(), &prepared.resources)?;
    let unresolved = completed.result()["output"]["unresolved"]
        .as_array()
        .is_some_and(|items| !items.is_empty());
    let path = match &prepared.subject {
        Subject::Queue {
            task_id,
            cache_hash,
            target_revision,
            ..
        } => {
            if unresolved {
                return Err(miette::miette!(
                    "unresolved work concerns remain; resolve with evidence or replan before queue publication"
                ));
            }
            let task = super::queue::current_task(session.root(), task_id)?;
            let (id, hash, revision) = match task.kind {
                lk_queue::TaskKind::ExtractConcepts => {
                    let typed: lk_queue::TaskResult =
                        serde_json::from_value(result.clone()).into_diagnostic()?;
                    let selected = prepared.context["purpose"]["extraction_scope"]
                        .get("selection")
                        .map(|value| {
                            serde_json::from_value::<lk_queue::ExtractionSelection>(value.clone())
                        })
                        .transpose()
                        .into_diagnostic()?;
                    if typed.selection != selected {
                        return Err(miette::miette!(
                            "final extraction selection differs from its captured evidence scope"
                        ));
                    }
                    if typed.context_revision != work.plan.subject_revision {
                        return Err(miette::miette!(
                            "extraction context revision does not match the work read scope"
                        ));
                    }
                    (typed.task_id, typed.cache_hash, typed.target_revision)
                }
                lk_queue::TaskKind::SynthesizeConcept => {
                    let typed: lk_queue::SynthesisResult =
                        serde_json::from_value(result.clone()).into_diagnostic()?;
                    if typed.context_revision != work.plan.subject_revision {
                        return Err(miette::miette!(
                            "synthesis context revision does not match the work read scope"
                        ));
                    }
                    (typed.task_id, typed.cache_hash, typed.target_revision)
                }
                _ => {
                    let typed: lk_queue::SectionResult =
                        serde_json::from_value(result.clone()).into_diagnostic()?;
                    (typed.task_id, typed.cache_hash, typed.target_revision)
                }
            };
            if &id != task_id || &hash != cache_hash || &revision != target_revision {
                return Err(miette::miette!(
                    "final result does not match the exact work subject"
                ));
            }
            queue_result_path(task.kind, &result)?
        }
        Subject::Audit {
            path,
            file_revision,
            input_revision,
        } => {
            let typed: lk_graph::audit::AuditResults =
                serde_json::from_value(result.clone()).into_diagnostic()?;
            if typed.assessments.len() != 1
                || typed.assessments[0].path != *path
                || typed.assessments[0].file_revision != *file_revision
                || typed.assessments[0].input_revision != *input_revision
            {
                return Err(miette::miette!(
                    "final assessment does not match the exact work subject"
                ));
            }
            if unresolved
                && typed.assessments[0].disposition != lk_graph::audit::AuditDisposition::FollowUp
            {
                return Err(miette::miette!(
                    "unresolved work concerns require a follow-up assessment with a scoped open review"
                ));
            }
            PathBuf::from(".lorekeeper/work")
                .join(&work.plan.id)
                .join("assessment.json")
        }
    };
    let mut mutation = lk_vault::MutationPlan::default();
    prepared.guard_pages(&mut mutation);
    completed.plan_seal(&mut mutation, session.root(), &result)?;
    let raw = serde_json::to_string(&result).into_diagnostic()?;
    let final_path = PathBuf::from(".lorekeeper/work")
        .join(&work.plan.id)
        .join("final.json");
    let final_before = match std::fs::read_to_string(session.root().join(&final_path)) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(miette::miette!("read final result: {error}")),
    };
    mutation
        .publish_expected(session.root(), final_path, final_before, raw.clone())
        .into_diagnostic()?;
    let before = match std::fs::read_to_string(session.root().join(&path)) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(miette::miette!("read staged result: {error}")),
    };
    mutation
        .publish_expected(session.root(), &path, before, raw)
        .into_diagnostic()?;
    Ok((path, mutation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::concept::file_revision;
    use lk_vault::{MutationPlan, MutationSession};

    fn write(root: &Path, path: &str, raw: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, raw).unwrap();
    }

    fn completed_audit_work(root: &Path) -> (Work, Prepared, Value) {
        let config_path = root.join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "vault:\n  root: {}\n  locale: en\nidentity: {{name: Tester, email: tester@example.com}}\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\nexecution:\n  work: {{max_input_bytes: 8192, max_result_bytes: 2048}}\n",
                serde_json::to_string(root).unwrap()
            ),
        )
        .unwrap();
        let (config, config_revision) =
            lk_core::config::Config::load_with_revision(&config_path).unwrap();
        let path = "wiki/documents/subject.md";
        write(
            root,
            path,
            &format!(
                "---\ntype: document\ntitle: Subject\n---\n## Content\n{}",
                "A source-scoped observation.\n\n".repeat(500)
            ),
        );
        let prepared = resources::audit(root, &config, path).unwrap();
        let session = MutationSession::acquire(root).unwrap();
        let work = Work::create(
            &session,
            &prepared.resources,
            &prepared.revision(&config_revision).unwrap(),
            prepared.context.clone(),
            limits(&config).unwrap(),
        )
        .unwrap();
        assert!(work.plan.parts.len() > 1);
        loop {
            let (status, request) = work.prepare_inspection(root, &prepared.resources).unwrap();
            if status.complete {
                break;
            }
            let request = request.unwrap();
            work.record(
                &session,
                WorkOutput {
                    work_id: work.plan.id.clone(),
                    node_id: request["node_id"].as_str().unwrap().into(),
                    input_revision: request["input_revision"].as_str().unwrap().into(),
                    summary: "Fixture read of every exact assigned observation.".into(),
                    findings: vec![],
                    source_questions: vec![],
                    unresolved: vec![],
                    resolutions: vec![],
                },
                &prepared.resources,
            )
            .unwrap();
        }
        let Subject::Audit {
            path,
            file_revision,
            input_revision,
        } = &prepared.subject
        else {
            unreachable!()
        };
        let result = json!({"assessments":[{
            "path":path,"file_revision":file_revision,"input_revision":input_revision,
            "disposition":"reviewed","reason":"Fixture completed the exact selected reads.","reviews":[],
        }]});
        (work, prepared, result)
    }

    #[test]
    fn combined_completion_refuses_changed_manifest_or_observation_before_publication() {
        for artifact in ["plan.json", "observation"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let (work, prepared, result) = completed_audit_work(&root);
            let session = MutationSession::acquire(&root).unwrap();
            let (staged, mutation) = plan_finish(&session, &work, &prepared, result).unwrap();
            let work_dir = PathBuf::from(".lorekeeper/work").join(&work.plan.id);
            let changed = if artifact == "observation" {
                work_dir
                    .join("nodes")
                    .join(format!("{}.json", work.plan.parts[0].id))
            } else {
                work_dir.join(artifact)
            };
            let before = std::fs::read_to_string(root.join(&changed)).unwrap();
            let after = format!("{before}\n");
            std::fs::write(root.join(&changed), &after).unwrap();
            assert!(session.apply(mutation).is_err());
            assert_eq!(std::fs::read_to_string(root.join(&changed)).unwrap(), after);
            for path in [
                work_dir.join("seal.json"),
                work_dir.join("final.json"),
                staged,
            ] {
                assert!(!root.join(path).exists());
            }
            assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
        }
    }

    #[test]
    fn interrupted_combined_completion_recovers_seal_final_and_staged_result_together() {
        for seal_receipted in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let (work, prepared, result) = completed_audit_work(&root);
            let original = std::fs::read(root.join("wiki/documents/subject.md")).unwrap();
            let session = MutationSession::acquire(&root).unwrap();
            let (staged, mutation) =
                plan_finish(&session, &work, &prepared, result.clone()).unwrap();
            let work_dir = PathBuf::from(".lorekeeper/work").join(&work.plan.id);
            let seal = work_dir.join("seal.json");
            let plan_revision = file_revision(&serde_json::to_string(&mutation).unwrap());
            let plan = serde_json::to_value(mutation).unwrap();
            let steps = plan["steps"].as_array().unwrap();
            let seal_index = steps
                .iter()
                .position(|step| step["path"] == seal.to_str().unwrap())
                .unwrap();
            let seal_after = steps[seal_index]["after"].as_str().unwrap().to_owned();
            let completed = seal_index + usize::from(seal_receipted);
            drop(session);
            write(
                &root,
                ".lorekeeper/mutation/pending.json",
                &json!({"version":1,"plan":plan}).to_string(),
            );
            write(&root, seal.to_str().unwrap(), &seal_after);
            write(
                &root,
                ".lorekeeper/mutation/progress.json",
                &json!({
                    "plan_revision":plan_revision,"completed":completed,
                    "checksum":file_revision(&format!("{plan_revision}:{completed}")),
                })
                .to_string(),
            );
            assert!(!root.join(work_dir.join("final.json")).exists());
            assert!(!root.join(&staged).exists());
            let _recovered = MutationSession::acquire(&root).unwrap();
            for path in [work_dir.join("final.json"), staged] {
                let restored: Value =
                    serde_json::from_slice(&std::fs::read(root.join(path)).unwrap()).unwrap();
                assert_eq!(restored, result);
            }
            work.guard_completion(
                &mut MutationPlan::default(),
                &root,
                &prepared.resources,
                &result,
            )
            .unwrap();
            assert_eq!(
                std::fs::read(root.join("wiki/documents/subject.md")).unwrap(),
                original
            );
            for file in ["pending.json", "progress.json"] {
                assert!(!root.join(".lorekeeper/mutation").join(file).exists());
            }
            assert!(!root.join(".lorekeeper/knowledge-audit.json").exists());
        }
    }

    #[test]
    fn progressive_plan_matches_ready_scope_and_requires_recorded_file_recipes() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.yaml");
        std::fs::write(&config_path, "vault:\n  root: vault\n  locale: en\nidentity: {name: Tester, email: tester@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n    concept_policy: supported-new\nexecution:\n  work: {extraction_context: progressive, max_input_bytes: 16384, max_result_bytes: 4096, max_calls: 4}\n").unwrap();
        let (mut config, config_revision) =
            lk_core::config::Config::load_with_revision(&config_path).unwrap();
        std::fs::create_dir(directory.path().join("vault")).unwrap();
        let root = directory.path().join("vault").canonicalize().unwrap();
        let target = "wiki/documents/subject.md";
        let selected = "wiki/concepts/keep.md";
        let excluded = "wiki/documents/drop.md";
        let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "ext",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                rss_original: None,
                text: "Candidate source evidence.".into(),
                source_id: "notes".into(),
                source_type: lk_core::config::SourceType::Manual,
                date: jiff::civil::date(2026, 10, 22),
                focus: None,
                locale: lk_core::i18n::Locale::En,
                categories: vec![],
                target: lk_queue::TaskTarget {
                    vault_path: target.into(),
                    kind: lk_queue::TargetKind::DocumentConcepts,
                    anchor: "## Related Concepts".into(),
                },
            },
        );
        let target_raw = format!(
            "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nCandidate source evidence.\n\n## Related Concepts\n",
            serde_json::to_string(&task.cache_hash).unwrap()
        );
        write(&root, target, &target_raw);
        write(
            &root,
            selected,
            &format!(
                "---\ntype: concept\ntitle: Candidate owner\n---\n## Synthesis\n{}\n",
                "Candidate details. ".repeat(1200)
            ),
        );
        write(
            &root,
            excluded,
            &format!(
                "---\ntype: document\ntitle: Other observation\n---\n## Content\n{}\n",
                "Candidate unrelated. ".repeat(2000)
            ),
        );
        write(
            &root,
            ".lorekeeper/queue/fixture.jsonl",
            &serde_json::to_string(&task).unwrap(),
        );
        let queries = [lk_core::discovery::DiscoveryQuery {
            query: "Candidate".into(),
            phrase: true,
            spacing: false,
        }];
        assert_eq!(
            task_context(&root, &config, &config_revision, &task.task_id, None, None).unwrap()["status"],
            "needs-discovery"
        );
        assert!(
            task_context(
                &root,
                &config,
                &config_revision,
                &task.task_id,
                Some(&queries),
                None
            )
            .unwrap_err()
            .to_string()
            .contains("recorded recipes")
        );
        config.execution.work.extraction_context =
            lk_core::execution::ExtractionContext::FullContext;
        let legacy = task_context(
            &root,
            &config,
            &config_revision,
            &task.task_id,
            Some(&queries),
            None,
        )
        .unwrap();
        assert_eq!(legacy["status"], "ready");
        assert!(legacy["selection"].is_null());
        config.execution.work.extraction_context =
            lk_core::execution::ExtractionContext::Progressive;
        {
            let session = MutationSession::acquire(&root).unwrap();
            discovery::record(
                &session,
                &task.task_id,
                serde_json::from_value(json!({
                    "target_revision":file_revision(&target_raw),"queries":queries,
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let different = [lk_core::discovery::DiscoveryQuery {
            query: "Different".into(),
            ..queries[0].clone()
        }];
        assert!(
            task_context(
                &root,
                &config,
                &config_revision,
                &task.task_id,
                Some(&different),
                None
            )
            .unwrap_err()
            .to_string()
            .contains("differs from current recorded recipes")
        );
        assert!(
            task_for_plan(
                &root,
                &config,
                &config_revision,
                &task.task_id,
                Some(&queries)
            )
            .err()
            .unwrap()
            .to_string()
            .contains("screening is incomplete")
        );
        {
            let session = MutationSession::acquire(&root).unwrap();
            current_screening(&root, &config, &config_revision, &task.task_id)
                .unwrap()
                .persist(&session, &config, &config_revision)
                .unwrap();
        }
        let turn = prepare_screening(&root, &config, &config_revision, &task.task_id).unwrap();
        let request = turn.request.unwrap();
        let decisions = request["candidates"].as_array().unwrap().iter().map(|candidate| {
            json!({"candidate_key":candidate["candidate_key"],
                "decision":if candidate["candidate_key"] == selected {"inspect"} else {"not-selected"},
                "reason":"Decide this source-scoped card before reading selected evidence."})
        }).collect::<Vec<_>>();
        record_screening(
            &root,
            &config,
            &config_revision,
            &task.task_id,
            json!({
                "selection_id":request["selection_id"],"batch_id":request["batch_id"],
                "input_revision":request["input_revision"],"decisions":decisions,
            }),
            None,
        )
        .unwrap();
        let ready = task_context(
            &root,
            &config,
            &config_revision,
            &task.task_id,
            Some(&queries),
            None,
        )
        .unwrap();
        assert_eq!(ready["status"], "ready");
        assert_eq!(ready["requires_bounded_work"], true);
        let session = MutationSession::acquire(&root).unwrap();
        let prepared = task_for_plan(
            &root,
            &config,
            &config_revision,
            &task.task_id,
            Some(&queries),
        )
        .unwrap();
        assert_eq!(
            prepared.revision(&config_revision).unwrap(),
            ready["context_revision"].as_str().unwrap()
        );
        assert_eq!(prepared.context, ready["context"]);
        assert!(
            prepared
                .resources
                .iter()
                .any(|resource| resource.address == selected)
        );
        assert!(
            !prepared
                .resources
                .iter()
                .any(|resource| resource.address == excluded)
        );
        assert!(prepared.pages.contains_key(Path::new(excluded)));
        let work = Work::create(
            &session,
            &prepared.resources,
            &prepared.revision(&config_revision).unwrap(),
            prepared.context,
            limits(&config).unwrap(),
        )
        .unwrap();
        assert!(work.plan.parts.len() > 1);
        assert_eq!(
            std::fs::read_to_string(root.join(target)).unwrap(),
            target_raw
        );
    }

    #[test]
    fn recording_prepared_inspection_refreshes_changed_and_new_discovery_context() {
        for added_candidate in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let config_path = directory.path().join("config.yaml");
            std::fs::write(&config_path, "vault:\n  root: vault\n  locale: en\nidentity: {name: Tester, email: tester@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n    concept_policy: supported-new\nexecution:\n  work: {extraction_context: full-context, max_input_bytes: 8192, max_result_bytes: 1024, max_calls: 1}\n").unwrap();
            let (config, config_revision) =
                lk_core::config::Config::load_with_revision(&config_path).unwrap();
            std::fs::create_dir(directory.path().join("vault")).unwrap();
            let root = directory.path().join("vault").canonicalize().unwrap();
            let target = "wiki/documents/subject.md";
            let context = "wiki/documents/context.md";
            let added = "wiki/documents/new-context.md";
            let client = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue"));
            let task = client.build_task(
                "ext",
                lk_queue::TaskKind::ExtractConcepts,
                lk_queue::ExtractConceptsRequest {
                    concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                    rss_original: None,
                    text: "Selected Candidate has source-scoped evidence.\n\n".repeat(400),
                    source_id: "notes".into(),
                    source_type: lk_core::config::SourceType::Manual,
                    date: jiff::civil::date(2026, 10, 20),
                    focus: None,
                    locale: lk_core::i18n::Locale::En,
                    categories: vec![],
                    target: lk_queue::TaskTarget {
                        vault_path: target.into(),
                        kind: lk_queue::TargetKind::DocumentConcepts,
                        anchor: "## Related Concepts".into(),
                    },
                },
            );
            let subject = format!(
                "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nSelected Candidate is the exact subject.\n\n## Related Concepts\n",
                task.cache_hash,
            );
            let original_context =
                "---\ntype: document\n---\nSelected Candidate has separate context.\n";
            write(&root, target, &subject);
            write(&root, context, original_context);
            write(
                &root,
                ".lorekeeper/queue/fixture.jsonl",
                &serde_json::to_string(&task).unwrap(),
            );
            {
                let session = MutationSession::acquire(&root).unwrap();
                discovery::record(
                    &session,
                    &task.task_id,
                    serde_json::from_value(json!({
                        "target_revision":file_revision(&subject),
                        "queries":[{"query":"Selected Candidate","phrase":true,"spacing":false}],
                    }))
                    .unwrap(),
                )
                .unwrap();
            }
            let TaskWork::Partitioned { id } =
                prepare_task(&root, &config, &config_revision, &task.task_id).unwrap()
            else {
                panic!("fixture must require bounded work");
            };
            let (before, input) =
                prepare_inspection(&root, &config, &config_revision, &id).unwrap();
            let input = input.unwrap();
            let node = before.ready.first().unwrap();
            assert_eq!(input["node_id"].as_str(), Some(node.as_str()));
            let output = WorkOutput {
                work_id: id.clone(),
                node_id: node.clone(),
                input_revision: input["input_revision"].as_str().unwrap().into(),
                summary: "Read the assigned exact source ranges.".into(),
                findings: vec![],
                source_questions: vec![],
                unresolved: vec![],
                resolutions: vec![],
            };
            if added_candidate {
                write(
                    &root,
                    added,
                    "---\ntype: document\n---\nSelected Candidate is newly discoverable.\n",
                );
            } else {
                write(
                    &root,
                    context,
                    "---\ntype: document\n---\nSelected Candidate context changed after inspection.\n",
                );
            }
            assert!(
                record_inspection(&root, &config, &config_revision, &id, node, output.clone())
                    .is_err()
            );
            assert!(
                !root
                    .join(".lorekeeper/work")
                    .join(&id)
                    .join("nodes")
                    .join(format!("{node}.json"))
                    .exists()
            );
            if added_candidate {
                std::fs::remove_file(root.join(added)).unwrap();
            } else {
                write(&root, context, original_context);
            }
            let after =
                record_inspection(&root, &config, &config_revision, &id, node, output).unwrap();
            assert!(after.completed > before.completed);
            assert!(!after.ready.contains(node));
        }
    }

    #[test]
    fn batch_reuse_scopes_recover_owned_origins_and_preserve_external_read_guards() {
        for (accepted, changed_context, changed_unpublished) in
            [(2, false, false), (2, true, false), (1, false, true)]
        {
            let directory = tempfile::tempdir().unwrap();
            let config_path = directory.path().join("config.yaml");
            std::fs::write(&config_path, "vault:\n  root: vault\n  locale: en\nidentity:\n  name: Tester\n  email: tester@example.com\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n").unwrap();
            let (config, config_revision) =
                lk_core::config::Config::load_with_revision(&config_path).unwrap();
            std::fs::create_dir_all(directory.path().join("vault")).unwrap();
            let root = directory.path().join("vault").canonicalize().unwrap();
            let owner = "wiki/concepts/canonical.md";
            let context = "wiki/documents/context.md";
            let queue = root.join(".lorekeeper/queue");
            let client = lk_queue::QueueLlmClient::new(queue.clone());
            let mut sources = Vec::new();
            let mut tasks = Vec::new();
            for target in ["wiki/documents/source-a.md", "wiki/documents/source-b.md"] {
                let task = client.build_task(
                    "ext",
                    lk_queue::TaskKind::ExtractConcepts,
                    lk_queue::ExtractConceptsRequest {
                        concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                        rss_original: None,
                        text: format!("Selected Candidate has source-scoped evidence in {target}."),
                        source_id: "notes".into(),
                        source_type: lk_core::config::SourceType::Manual,
                        date: jiff::civil::date(2026, 10, 20),
                        focus: None,
                        locale: lk_core::i18n::Locale::En,
                        categories: vec![],
                        target: lk_queue::TaskTarget {
                            vault_path: target.into(),
                            kind: lk_queue::TargetKind::DocumentConcepts,
                            anchor: "## Related Concepts".into(),
                        },
                    },
                );
                let before = format!(
                    "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nSelected Candidate has attributed source material.\n\n## Related Concepts\n",
                    task.cache_hash
                );
                let linked = format!("{before}\n[Selected Candidate](../concepts/canonical.md)\n");
                let after = lk_vault::set_llm_input(
                    &linked,
                    "concepts_done",
                    &serde_json::to_string(&task.cache_hash).unwrap(),
                )
                .unwrap();
                write(&root, target, &before);
                sources.push((target, before, after));
                tasks.push(task);
            }
            let owner_before = "---\ntype: concept\ntitle: Selected Candidate\n---\n## Synthesis\nExisting scoped interpretation.\n";
            let context_before =
                "---\ntype: document\n---\nSelected Candidate has separate context.\n";
            write(&root, owner, owner_before);
            write(&root, context, context_before);
            write(
                &root,
                ".lorekeeper/queue/fixture.jsonl",
                &tasks
                    .iter()
                    .map(|task| serde_json::to_string(task).unwrap())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            let discovery = vec![lk_core::discovery::DiscoveryQuery {
                query: "Selected Candidate".into(),
                phrase: true,
                spacing: false,
            }];
            let session = MutationSession::acquire(&root).unwrap();
            let mut guards = Vec::new();
            for (index, task) in tasks.iter().take(accepted).enumerate() {
                let prepared =
                    resources::task_with_discovery(&root, &config, &task.task_id, &discovery)
                        .unwrap();
                assert!(
                    prepared
                        .resources
                        .iter()
                        .any(|resource| resource.address == sources[1 - index].0),
                    "each origin is in the other result's discovery scope"
                );
                let result = lk_queue::TaskResult {
                    selection: None,
                    concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                    task_id: task.task_id.clone(),
                    cache_hash: task.cache_hash.clone(),
                    target_revision: file_revision(&sources[index].1),
                    context_revision: prepared.revision(&config_revision).unwrap(),
                    discovery: discovery.clone(),
                    target: task.target.clone(),
                    date: jiff::civil::date(2026, 10, 20),
                    concepts: vec![lk_queue::ReportedConcept {
                        admission: lk_queue::ConceptAdmission::Reuse {
                            owner: lk_queue::ConceptOwner {
                                path: owner.into(),
                                revision: file_revision(owner_before),
                            },
                        },
                        concept: lk_core::concept::ExtractedConcept {
                            name: "Selected Candidate".into(),
                            category: None,
                            aliases: vec![],
                        },
                        synthesis: None,
                        rss_evidence: vec![],
                    }],
                };
                guards.push(
                    prepare_queue_publication(
                        &root,
                        &config,
                        &config_revision,
                        &task.task_id,
                        serde_json::to_value(result).unwrap(),
                    )
                    .unwrap(),
                );
            }
            let mut owned =
                std::collections::BTreeMap::from([(PathBuf::from(owner), owner_before.to_owned())]);
            for (target, before, _) in sources.iter().take(accepted) {
                owned.insert(PathBuf::from(target), before.clone());
            }
            let mut plan = MutationPlan::default();
            for guard in guards {
                assert!(!guard.append_to(&mut plan, &owned).unwrap());
            }
            let owner_after = format!("{owner_before}\nAdditional attributed source context.\n");
            plan.edit(owner, owner_before.into(), owner_after.clone());
            for (target, before, after) in sources.iter().take(accepted) {
                plan.edit(target, before.clone(), after.clone());
            }
            let encoded = serde_json::to_value(&plan).unwrap();
            let steps = encoded["steps"].as_array().unwrap();
            let crash_step = steps
                .iter()
                .position(|step| {
                    step["path"] == sources[0].0
                        && step["kind"] == "edit"
                        && step["before"] != step["after"]
                })
                .unwrap();
            assert!(crash_step > 0);
            assert!(
                steps
                    .iter()
                    .any(|step| step["path"] == context && step["before"] == step["after"])
            );
            let revision = file_revision(&serde_json::to_string(&plan).unwrap());
            let progress = json!({"plan_revision":revision,"completed":crash_step,"checksum":file_revision(&format!("{revision}:{crash_step}"))});
            write(
                &root,
                ".lorekeeper/mutation/pending.json",
                &json!({"version":1,"plan":plan}).to_string(),
            );
            write(
                &root,
                ".lorekeeper/mutation/progress.json",
                &progress.to_string(),
            );
            write(&root, owner, &owner_after);
            write(&root, sources[0].0, &sources[0].2);
            if changed_context {
                write(
                    &root,
                    context,
                    "External context changed after the guarded read.\n",
                );
            }
            if changed_unpublished {
                write(
                    &root,
                    sources[1].0,
                    "An unpublished task's target changed externally.\n",
                );
            }
            drop(session);
            let recovered = MutationSession::acquire(&root);
            if changed_context || changed_unpublished {
                let error = recovered
                    .err()
                    .expect("changed unowned context must refuse recovery");
                assert!(error.to_string().contains(if changed_context {
                    context
                } else {
                    sources[1].0
                }));
                if changed_context {
                    assert_eq!(
                        std::fs::read_to_string(root.join(sources[1].0)).unwrap(),
                        sources[1].1
                    );
                }
                assert!(root.join(".lorekeeper/mutation/pending.json").exists());
                write(&root, context, context_before);
                write(&root, sources[1].0, &sources[1].1);
                let _recovered = MutationSession::acquire(&root).unwrap();
            } else {
                drop(recovered.unwrap());
            }
            assert_eq!(
                std::fs::read_to_string(root.join(owner)).unwrap(),
                owner_after
            );
            for (index, (target, before, after)) in sources.iter().enumerate() {
                assert_eq!(
                    std::fs::read_to_string(root.join(target)).unwrap(),
                    if index < accepted { after } else { before }.as_str()
                );
            }
            assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
        }
    }
}
