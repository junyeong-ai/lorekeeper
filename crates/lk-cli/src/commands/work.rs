use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{GlobalOptions, find_config, load_config_with_revision};

mod resources;
pub(crate) mod state;

use resources::{Prepared, Subject};
use state::{Work, WorkLimits, WorkOutput};

#[derive(clap::Subcommand)]
pub enum WorkCommand {
    /// Plan bounded evidence inspections without changing knowledge pages
    Plan {
        /// Exact current queue task identifier; mutually exclusive with --path
        #[arg(long, required_unless_present = "path", conflicts_with = "path")]
        task_id: Option<String>,
        /// Exact vault-relative concept or document address to audit
        #[arg(long, required_unless_present = "task_id")]
        path: Option<String>,
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
    prepared: &Prepared,
    config: &lk_core::config::Config,
    config_revision: &str,
) -> miette::Result<Work> {
    Work::build(
        &prepared.resources,
        &prepared.revision(config_revision)?,
        prepared.context.clone(),
        limits(config)?,
    )
}

fn guard_prepared(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    prepared: Prepared,
    result: Value,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<bool> {
    let expected = expected(&prepared, config, config_revision)?;
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

pub(super) fn guard_queue(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    result: Value,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<bool> {
    let root = root.canonicalize().into_diagnostic()?;
    let mut prepared = resources::task_for_publication(&root, config, task_id)?;
    if let Subject::Queue { target, .. } = &prepared.subject {
        let target = lk_vault::page_path(&root, Path::new(target))
            .into_diagnostic()?
            .canonicalize()
            .into_diagnostic()?;
        prepared
            .pages
            .remove(target.strip_prefix(&root).into_diagnostic()?);
    }
    guard_prepared(&root, config, config_revision, prepared, result, mutation)
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
    let prepared = resources::refresh(root, config, &work.context)?;
    if expected(&prepared, config, config_revision)? != work {
        return Err(miette::miette!(
            "work inputs, configuration or bounded execution policy changed; replan"
        ));
    }
    Ok((work, prepared))
}

pub(super) fn prepare_task(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<Option<String>> {
    let root = root.canonicalize().into_diagnostic()?;
    let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
    let prepared = resources::task(&root, config, task_id)?;
    let work = expected(&prepared, config, config_revision)?;
    if work.plan.parts.len() == 1 {
        return Ok(None);
    }
    Work::create(
        &session,
        &prepared.resources,
        &prepared.revision(config_revision)?,
        prepared.context,
        limits(config)?,
    )?;
    Ok(Some(work.plan.id))
}

pub(super) fn prepare_audit(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    path: &str,
) -> miette::Result<String> {
    let root = root.canonicalize().into_diagnostic()?;
    let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
    let prepared = resources::audit(&root, config, path)?;
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
    path: &str,
) -> miette::Result<bool> {
    let root = root.canonicalize().into_diagnostic()?;
    let prepared = resources::audit(&root, config, path)?;
    Ok(expected(&prepared, config, config_revision)?
        .plan
        .parts
        .len()
        > 1)
}

pub(super) fn current_progress(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<state::WorkProgress> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, prepared) = load(&root, config, config_revision, id)?;
    work.progress(&root, &prepared.resources)
}

pub(super) fn inspection_input(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    node: &str,
) -> miette::Result<Value> {
    let (work, prepared) = load(root, config, config_revision, id)?;
    work.input(root, node, &prepared.resources)
}

pub(super) fn record_inspection(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    node: &str,
    output: WorkOutput,
) -> miette::Result<()> {
    if output.work_id != id || output.node_id != node {
        return Err(miette::miette!(
            "observation does not answer the selected work node"
        ));
    }
    let session = lk_vault::MutationSession::acquire(root).into_diagnostic()?;
    let (work, prepared) = load(root, config, config_revision, id)?;
    work.record(&session, output, &prepared.resources)
        .map(|_| ())
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
        "resources":work.plan.resources.len(),"parts":work.plan.parts.len(),
        "completed":status.completed,"total":status.total,"complete":status.complete,
        "root_node":status.root_node,"ready_total":total_ready,"ready":ready,
        "limit":limit,"offset":offset,"next_offset":next_offset}))
}

pub async fn run(opts: &GlobalOptions, command: WorkCommand) -> miette::Result<()> {
    let (config, config_revision) = load_config_with_revision(&find_config(opts)?)?;
    let root = config.vault.root_path().canonicalize().into_diagnostic()?;
    let output = match command {
        WorkCommand::Plan { task_id, path } => {
            let session = lk_vault::MutationSession::acquire(&root).into_diagnostic()?;
            let prepared = match (task_id, path) {
                (Some(id), None) => resources::task(&root, &config, &id)?,
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
    let reduced = work.result(session.root(), &prepared.resources)?;
    let unresolved = reduced["output"]["unresolved"]
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
            let (id, hash, revision, suffix) = match task.kind {
                lk_queue::TaskKind::ExtractConcepts => {
                    let typed: lk_queue::TaskResult =
                        serde_json::from_value(result.clone()).into_diagnostic()?;
                    (
                        typed.task_id,
                        typed.cache_hash,
                        typed.target_revision,
                        ".json",
                    )
                }
                lk_queue::TaskKind::SynthesizeConcept => {
                    let typed: lk_queue::SynthesisResult =
                        serde_json::from_value(result.clone()).into_diagnostic()?;
                    (
                        typed.task_id,
                        typed.cache_hash,
                        typed.target_revision,
                        lk_queue::SYNTHESIS_RESULT_SUFFIX,
                    )
                }
                _ => {
                    let typed: lk_queue::SectionResult =
                        serde_json::from_value(result.clone()).into_diagnostic()?;
                    (
                        typed.task_id,
                        typed.cache_hash,
                        typed.target_revision,
                        lk_queue::SECTION_RESULT_SUFFIX,
                    )
                }
            };
            if &id != task_id || &hash != cache_hash || &revision != target_revision {
                return Err(miette::miette!(
                    "final result does not match the exact work subject"
                ));
            }
            let digest = blake3::hash(&serde_json::to_vec(&result).into_diagnostic()?)
                .to_hex()
                .to_string();
            PathBuf::from(".lorekeeper/queue/results").join(format!("{digest}{suffix}"))
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
    work.seal(session, &prepared.resources, result.clone())?;
    let mut mutation = lk_vault::MutationPlan::default();
    prepared.guard_pages(&mut mutation);
    work.guard_completion(&mut mutation, session.root(), &prepared.resources, &result)?;
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
    session.apply(mutation).into_diagnostic()?;
    Ok(json!({"work_id":work.plan.id,"staged_result":path,"canonical_published":false}))
}
