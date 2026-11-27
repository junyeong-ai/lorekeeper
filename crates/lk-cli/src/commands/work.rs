use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{GlobalOptions, find_config, load_config_with_revision};

#[cfg(unix)]
mod action;
#[cfg(unix)]
pub(super) mod audit;
mod discovery;
mod open_review;
pub(in crate::commands) use open_review::{
    PublishedReviews, PublishedState, open_reviews, published_state,
};
#[cfg(unix)]
pub(super) mod generation;
pub(super) mod host;
#[cfg(unix)]
pub(super) mod native;
pub(super) mod preparation;
mod publication;
mod receipt;
mod resources;
#[cfg(unix)]
mod review;
#[cfg(unix)]
pub(super) mod section;
mod selection;
mod snapshot;
mod source;
pub(crate) mod state;
pub(super) mod workload;

pub(super) use publication::publication_scope_controlled;
pub(super) use resources::graph_citation_digest;
#[cfg(unix)]
pub(super) use selection::ScreeningDecision;
pub(super) use selection::ScreeningRejection;
pub(super) use selection::retired as retired_selection;
#[cfg(unix)]
pub(super) const FINALIZATION_REQUEST_ENV: &str = "LORE_WORK_FINALIZATION_REQUEST";
#[cfg(unix)]
pub(super) const FINALIZATION_REVISION_ENV: &str = "LORE_WORK_FINALIZATION_REVISION";
use resources::{Prepared, Subject};
#[cfg(unix)]
pub(in crate::commands) use selection::{
    INSTRUCTION as SCREENING_INSTRUCTION, reply_schema as screening_reply_schema,
};
use state::{Work, WorkLimits, WorkOutput};

use super::queue::application::{
    RejectionKind, is_citations_changed, is_work_input_changed, rejection as publication_rejection,
};

#[derive(clap::Subcommand)]
#[command(defer = true)]
pub enum WorkCommand {
    /// Inspect the candidate roster and remaining screening cost without changing work
    Cost {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
    },
    /// Read the next bounded packet of undecided candidates; seal the roster once all are decided
    Select {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
        /// Issue the deferred decisions for explicit reassessment instead
        #[arg(long)]
        reassess: bool,
    },
    /// Record a decision for every candidate of one issued packet
    SelectRecord {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
        /// JSON decisions answering the packet's exact request revision
        #[arg(long)]
        file: PathBuf,
    },
    /// Clear only the current packet's operational hold after diagnosing its failure
    SelectRetry {
        /// Exact retained concept-extraction task identifier
        #[arg(long)]
        task_id: String,
        /// Validate the current hold without publishing or recovering state
        #[arg(long)]
        dry_run: bool,
    },
    /// Clear one retained preparation hold without invoking an agent
    RetryPreparation {
        /// Preparation role that owns the retained attempt
        #[arg(long, value_enum)]
        stage: preparation::Stage,
        /// Exact retained queue task for extraction discovery or a whole section
        #[arg(long, required_unless_present = "path", conflicts_with = "path")]
        task_id: Option<String>,
        /// Exact vault-relative subject for audit discovery, screening or assessment
        #[arg(long, required_unless_present = "task_id", conflicts_with = "task_id")]
        path: Option<String>,
        /// Exact retained preparation input hash reported by the failed run
        #[arg(long)]
        input_revision: String,
        /// Validate the retained attempt and subject without publishing or recovering state
        #[arg(long)]
        dry_run: bool,
    },
    /// Clear one current native generation hold without invoking an agent
    RetryGeneration {
        /// Exact retained queue task
        #[arg(long)]
        task_id: String,
        /// Exact retained input revision reported by the held proposal or failed invocation
        #[arg(long)]
        input_revision: String,
        /// Validate without publishing or recovering state
        #[arg(long)]
        dry_run: bool,
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
    /// Read a current page of a Work's planned resource without recording an observation or completion
    Source {
        /// Work identifier pinning the task source and its complete current scope
        work_id: String,
        /// Planned resource index to read; defaults to the Work's retained task source
        #[arg(long)]
        resource: Option<usize>,
        /// Half-open Unicode scalar start, also the starting position for literal find
        #[arg(long, default_value = "0")]
        offset: usize,
        /// Maximum Unicode scalars to return (1..80000); complete JSON also obeys the Work input bound
        #[arg(long, default_value = "4000")]
        limit: usize,
        /// Seek the first exact literal match at or after offset; matches are not event identities
        #[arg(long)]
        find: Option<String>,
    },
    /// Refine a held inspection on Unix without changing its canonical plan or paid observations
    Refine {
        /// Current audit Work identifier returned by plan
        work_id: String,
        /// Unfinished ready inspection node with an exact native processing action
        node_id: String,
        /// Maximum Unicode scalar characters per smaller inspection assignment
        #[arg(long)]
        max_part_characters: usize,
        /// Validate the exact refinement without publishing or recovering state
        #[arg(long)]
        dry_run: bool,
    },
    /// Clear a current inspection's operational hold after diagnosing its failed invocation
    Retry {
        /// Exact current Work identifier retaining the operational attempt
        work_id: String,
        /// Exact unfinished inspection or reduction node with a failed or exhausted attempt
        node_id: String,
        /// Validate the exact retry without publishing or recovering state
        #[arg(long)]
        dry_run: bool,
    },
    /// Suspend an exact unfinished inspection without running an agent or granting refinement
    Hold {
        /// Exact current Work identifier
        work_id: String,
        /// Exact unfinished inspection or reduction node
        node_id: String,
        /// Operational reason, at most 256 compact JSON bytes
        #[arg(long)]
        reason: String,
        /// Validate current input guards without publishing or recovering state
        #[arg(long)]
        dry_run: bool,
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
        /// Exact native finalizer handoff captured before drafting
        #[arg(long)]
        prepared: Option<PathBuf>,
    },
}

pub(crate) fn limits(config: &lk_core::config::Config) -> miette::Result<WorkLimits> {
    let work = config.execution.effective_work().into_diagnostic()?;
    Ok(WorkLimits {
        max_input_bytes: work.max_input_bytes,
        max_result_bytes: work.max_result_bytes,
    })
}

/// The Work this preparation continues, and `prepared` takes the reading it was planned
/// under: a stored Work under the semantic reading; otherwise a Work planned before 0.144.0
/// on exact context bytes, with its observations and holds, while every page it read keeps
/// those bytes; otherwise the semantic reading plans one. A named Work and a drafted result
/// are checked against this one choice, so a Work that can run can also publish.
fn expected(
    root: &Path,
    prepared: &mut Prepared,
    config: &lk_core::config::Config,
    config_revision: &str,
) -> miette::Result<Work> {
    let semantic = prepared.read_as(resources::ContextReading::Semantic)?;
    let planned_semantic = match planned(root, &semantic, config, config_revision) {
        Ok(Planned::Stored(work)) => {
            *prepared = semantic;
            return Ok(work);
        }
        Ok(Planned::New(work)) => Ok(work),
        Err(error) if state::is_input_capacity(&error) => Err(error),
        Err(error) => return Err(error),
    };
    let exact = prepared.read_as(resources::ContextReading::Exact)?;
    if exact.resources != semantic.resources {
        // A reading that does not fit the current input bound cannot have a stored Work under
        // it, so its capacity error leaves the other reading to decide.
        match planned(root, &exact, config, config_revision) {
            Ok(Planned::Stored(work)) => {
                *prepared = exact;
                return Ok(work);
            }
            Ok(Planned::New(_)) => {}
            Err(error) if state::is_input_capacity(&error) => {}
            Err(error) => return Err(error),
        }
    }
    *prepared = semantic;
    planned_semantic
}

/// The Work one reading of a preparation plans.
enum Planned {
    /// Stored under any partition policy.
    Stored(Work),
    /// Not stored; the current partition policy plans it.
    New(Work),
}

fn planned(
    root: &Path,
    prepared: &Prepared,
    config: &lk_core::config::Config,
    config_revision: &str,
) -> miette::Result<Planned> {
    let expected =
        |partition| expected_partitioned(root, prepared, config, config_revision, partition);
    let byte_only = expected(state::Partition::Bytes)?;
    if byte_only.is_planned(root)? {
        return Ok(Planned::Stored(byte_only));
    }
    let current = expected(state::Partition::Inspection)?;
    if current.is_planned(root)? {
        return Ok(Planned::Stored(current));
    }
    // Work planned before page alignment continues under its byte-filled parts.
    let byte_filled = expected(state::Partition::LegacyInspection)?;
    Ok(if byte_filled.is_planned(root)? {
        Planned::Stored(byte_filled)
    } else {
        Planned::New(current)
    })
}

fn expected_partitioned(
    root: &Path,
    prepared: &Prepared,
    config: &lk_core::config::Config,
    config_revision: &str,
    partition: state::Partition,
) -> miette::Result<Work> {
    let current = Work::build_partitioned(
        &prepared.resources,
        &prepared.revision(config_revision)?,
        prepared.context.clone(),
        limits(config)?,
        partition,
    )?
    .with_saved_policy(root)?;
    if current.is_planned(root)? {
        return Ok(current);
    }
    let legacy_revision = prepared.legacy_revision(config_revision)?;
    if legacy_revision == current.plan.subject_revision {
        return Ok(current);
    }
    let Some(legacy_id) = current
        .plan
        .lookup_identity(&legacy_revision)
        .into_diagnostic()?
    else {
        return Err(miette::miette!(
            "configuration identity changed the bounded plan envelope"
        ));
    };
    let path = lk_vault::page_path(
        root,
        &Path::new(".lorekeeper/work")
            .join(legacy_id)
            .join("plan.json"),
    )
    .into_diagnostic()?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
        Err(error) => return Err(error).into_diagnostic(),
    }
    let legacy = Work::build_partitioned(
        &prepared.resources,
        &legacy_revision,
        prepared.context.clone(),
        limits(config)?,
        partition,
    )?
    .with_saved_policy(root)?;
    if legacy.is_planned(root)? {
        Ok(legacy)
    } else {
        Ok(current)
    }
}

fn persist_prepared(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    prepared: &mut Prepared,
) -> miette::Result<Work> {
    let work = expected(session.root(), prepared, config, config_revision)?;
    let created = !work.is_planned(session.root())?;
    let mut snapshot = lk_vault::MutationPlan::default();
    prepared.plan_snapshot(session.root(), &mut snapshot)?;
    let work = work.persist(session, snapshot)?;
    if created {
        work.reuse_recorded(session, &prepared.resources, |mutation| {
            prepared.guard_pages(mutation)
        })?;
    }
    Ok(work)
}

/// Guard `result` through `expected`, the Work `prepared` continues (`expected()`).
fn guard_prepared(
    root: &Path,
    expected: Work,
    prepared: Prepared,
    result: Value,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<bool> {
    let manifest = root
        .join(".lorekeeper/work")
        .join(&expected.plan.id)
        .join("plan.json");
    match std::fs::symlink_metadata(&manifest) {
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && expected.plan.parts.len() == 1 =>
        {
            prepared.guard_beyond_pages(mutation);
            return Ok(false);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(publication_rejection(
                RejectionKind::EvidenceScopeChanged,
                "bounded evidence work is required before publication; plan work for this exact subject",
            ));
        }
        Err(error) => return Err(miette::miette!("inspect bounded work: {error}")),
        Ok(_) => {}
    }
    let work = Work::load(root, &expected.plan.id)?;
    if work != expected {
        return Err(publication_rejection(
            RejectionKind::SemanticContextChanged,
            "bounded work manifest differs from current inputs",
        ));
    }
    work.guard_completion(mutation, root, &prepared.resources, &result)
        .map_err(|error| {
            if error.downcast_ref::<state::CompletionRefusal>().is_some() {
                publication_rejection(RejectionKind::EvidenceScopeChanged, error.to_string())
            } else {
                error
            }
        })?;
    if matches!(prepared.subject, Subject::Audit { .. }) {
        prepared.validate_audit_result(&result)?;
    }
    work.plan_publication(mutation, root, &result)?;
    prepared.guard_pages(mutation);
    Ok(true)
}

pub(super) struct QueuePublicationGuard {
    work: lk_vault::MutationPlan,
    pages: std::collections::BTreeMap<PathBuf, String>,
    owned_pages: std::collections::BTreeSet<PathBuf>,
    bounded: bool,
    /// The review callouts a synthesis publishes beneath its body, and their record.
    reviews: PublishedReviews,
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
                    return Err(publication_rejection(
                        RejectionKind::EvidenceScopeChanged,
                        format!(
                            "publication input differs from captured read context: {}",
                            path.display()
                        ),
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

/// Add the guards application needs for `result` and return the review callouts a synthesis
/// publishes beneath its body and their record, from the same publication capture.
pub(super) fn guard_queue(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    result: Value,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<PublishedReviews> {
    let mut guard = prepare_queue_publication(root, config, config_revision, task_id, result)?;
    let reviews = std::mem::take(&mut guard.reviews);
    let owned = guard
        .pages
        .iter()
        .filter(|(path, _)| guard.owned_pages.contains(*path))
        .map(|(path, raw)| (path.clone(), raw.clone()))
        .collect();
    guard.append_to(mutation, &owned)?;
    Ok(reviews)
}

/// Add the guards application would need for `result`. `sealing` is the candidate roster
/// this plan stores beside a freshly drafted extraction result.
#[allow(clippy::too_many_arguments)]
fn guard_queue_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    result: Value,
    sealing: Option<&selection::RosterSeal>,
    mutation: &mut lk_vault::MutationPlan,
    read: &lk_vault::ReadControl,
) -> miette::Result<bool> {
    let guard = prepare_queue_publication_controlled(
        root,
        config,
        config_revision,
        task_id,
        result,
        sealing,
        read,
    )?;
    if let Some(seal) = sealing {
        seal.plan(root, mutation)?;
    }
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
    prepare_queue_publication_controlled(
        root,
        config,
        config_revision,
        task_id,
        result,
        None,
        &lk_vault::ReadControl::default(),
    )
}

/// The review callouts a synthesis result publishes beneath its synthesis and their record,
/// checked against the open reviews of the concept this publication capture holds.
fn published_reviews(prepared: &Prepared, result: &Value) -> miette::Result<PublishedReviews> {
    let binding = prepared.result_binding()?;
    let list = |key: &str| -> miette::Result<Option<Vec<String>>> {
        match result.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(values) => serde_json::from_value(values.clone()).into_diagnostic(),
        }
    };
    let (refs, claims) = (list("review_refs")?, list("gap_claims")?);
    if let Err(reason) =
        open_review::check_reply(&binding.open_reviews, refs.as_deref(), claims.as_deref())
    {
        return Err(publication_rejection(RejectionKind::InvalidSection, reason));
    }
    let Subject::Queue { target, .. } = &prepared.subject else {
        return Ok(PublishedReviews::default());
    };
    let refs = refs.unwrap_or_default();
    Ok(PublishedReviews {
        callout: open_review::render(
            open_review::locale(&prepared.context)?,
            target,
            &binding.open_reviews,
            &refs,
            claims.as_deref().unwrap_or_default(),
        ),
        record: Some(open_review::record(&binding.open_reviews, &refs)),
    })
}

fn prepare_queue_publication_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    result: Value,
    sealing: Option<&selection::RosterSeal>,
    read: &lk_vault::ReadControl,
) -> miette::Result<QueuePublicationGuard> {
    let root = root.canonicalize().into_diagnostic()?;
    let mut prepared = resources::task_for_publication_controlled(&root, config, task_id, read)?;
    let mut selection_guards = lk_vault::MutationPlan::default();
    let mut owned_pages = std::collections::BTreeSet::new();
    let scoped_extraction = prepared.context["purpose"]["task_kind"]
        == serde_json::to_value(lk_queue::TaskKind::ExtractConcepts).into_diagnostic()?;
    if scoped_extraction {
        let typed: lk_queue::TaskResult =
            serde_json::from_value(result.clone()).into_diagnostic()?;
        if !typed.concepts.is_empty() && typed.discovery.is_empty() {
            return Err(publication_rejection(
                RejectionKind::EvidenceScopeChanged,
                "nonempty concept extraction requires discovery before drafting",
            ));
        }
        if let Some(selection) = &typed.selection {
            let sealed = sealed_task_controlled(
                &root,
                config,
                config_revision,
                &prepared,
                &typed.discovery,
                selection,
                sealing,
                read,
            )?;
            sealed.roster.guard(&mut selection_guards);
            prepared = sealed.prepared;
            prepared.pages.extend(sealed.observation_guards);
        } else {
            prepared = resources::with_discovery_controlled(
                &root,
                config,
                prepared,
                &typed.discovery,
                read,
            )?;
        }
        for concept in &typed.concepts {
            if let lk_queue::ConceptAdmission::Reuse { owner } = &concept.admission {
                let canonical = match lk_vault::page_path(&root, Path::new(&owner.path))
                    .and_then(|path| path.canonicalize())
                {
                    Ok(path) => path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(publication_rejection(
                            RejectionKind::EvidenceScopeChanged,
                            "selected concept owner is missing from the current evidence scope; reread current discovery and regenerate the result",
                        ));
                    }
                    Err(error) => return Err(error).into_diagnostic(),
                };
                let address = canonical
                    .strip_prefix(&root)
                    .into_diagnostic()?
                    .to_string_lossy();
                if !prepared.is_reuse_owner(address.as_ref(), &owner.revision) {
                    return Err(publication_rejection(
                        RejectionKind::EvidenceScopeChanged,
                        "selected concept owner is outside the captured discovery context or its read revision changed",
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
    if scoped_synthesis {
        prepared.require_requested_citations()?;
    }
    if !scoped_synthesis && !scoped_extraction {
        let section: lk_queue::SectionResult =
            serde_json::from_value(result.clone()).into_diagnostic()?;
        if let Some(revision) = section.context_revision {
            let scope = resources::section_task_controlled(&root, config, task_id, read)?;
            if revision != scope.revision(config_revision)?
                && revision != scope.legacy_revision(config_revision)?
            {
                return Err(publication_rejection(
                    RejectionKind::SemanticContextChanged,
                    "section review/read scope changed; reread and regenerate the result",
                ));
            }
            prepared.absent.extend(scope.absent.iter().cloned());
            for (path, content) in scope.pages {
                if prepared
                    .pages
                    .get(&path)
                    .is_some_and(|before| before != &content)
                {
                    return Err(publication_rejection(
                        RejectionKind::SemanticContextChanged,
                        "section input changed during review scope capture",
                    ));
                }
                prepared.pages.insert(path, content);
            }
        }
    }
    // The reading is decided while the page snapshots it is rebuilt from are still here.
    let expected_work = expected(&root, &mut prepared, config, config_revision)?;
    if scoped_synthesis || scoped_extraction {
        let revision = prepared.revision(config_revision)?;
        let supplied = result.get("context_revision").and_then(Value::as_str);
        if supplied != Some(revision.as_str())
            && supplied != Some(prepared.legacy_revision(config_revision)?.as_str())
        {
            return Err(publication_rejection(
                RejectionKind::SemanticContextChanged,
                "semantic read scope changed or was not captured before drafting; reread current context and regenerate the result",
            ));
        }
    }
    let reviews = if scoped_synthesis {
        published_reviews(&prepared, &result)?
    } else {
        PublishedReviews::default()
    };
    if scoped_extraction && !prepared.context["purpose"]["source_observations"].is_null() {
        let typed: lk_queue::TaskResult =
            serde_json::from_value(result.clone()).into_diagnostic()?;
        let mut source = resources::source_task_controlled(&root, config, task_id, read)?;
        let source_work = expected(&root, &mut source, config, config_revision)?;
        if prepared.context["purpose"]["source_observations"]["work_id"] != source_work.plan.id {
            return Err(publication_rejection(
                RejectionKind::SemanticContextChanged,
                "retained source observations differ from their current work identity",
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
    let bounded = guard_prepared(&root, expected_work, prepared, result, &mut work)?;
    Ok(QueuePublicationGuard {
        work,
        pages,
        owned_pages,
        bounded,
        reviews,
    })
}

fn guard_section_context(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    prepared: &Prepared,
    result: &Value,
    mutation: &mut lk_vault::MutationPlan,
) -> miette::Result<()> {
    let Subject::Queue { task_id, .. } = &prepared.subject else {
        return Ok(());
    };
    let kind: lk_queue::TaskKind =
        serde_json::from_value(prepared.context["purpose"]["task_kind"].clone())
            .into_diagnostic()?;
    if matches!(
        kind,
        lk_queue::TaskKind::ExtractConcepts | lk_queue::TaskKind::SynthesizeConcept
    ) {
        return Ok(());
    }
    let result: lk_queue::SectionResult =
        serde_json::from_value(result.clone()).into_diagnostic()?;
    let Some(revision) = &result.context_revision else {
        return Ok(());
    };
    let scope = resources::section_task_controlled(root, config, task_id, prepared.read_control())?;
    if *revision != scope.revision(config_revision)?
        && *revision != scope.legacy_revision(config_revision)?
    {
        return Err(publication_rejection(
            RejectionKind::SemanticContextChanged,
            "section review/read scope changed; reread and regenerate the result",
        ));
    }
    scope.guard_pages(mutation);
    Ok(())
}

#[cfg(unix)]
pub(super) fn plan_section_recovery(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
    result: &lk_queue::SectionResult,
    mutation: &mut lk_vault::MutationPlan,
    read: &lk_vault::ReadControl,
) -> miette::Result<Option<(String, std::collections::BTreeMap<PathBuf, String>)>> {
    let Some(revision) = &result.context_revision else {
        return Ok(None);
    };
    let scope = resources::section_task_controlled(root, config, &task.task_id, read)?;
    let current = scope.revision(config_revision)?;
    if *revision == current || *revision == scope.legacy_revision(config_revision)? {
        return Ok(None);
    }
    let mut prepared = resources::task_controlled(root, config, &task.task_id, read)?;
    let work = expected(root, &mut prepared, config, config_revision)?;
    let mut completions = std::collections::BTreeMap::new();
    if work.is_planned(root)? {
        if work.plan.max_part_characters.is_none() {
            return Ok(None);
        }
        let directory = Path::new(".lorekeeper/work").join(&work.plan.id);
        let publication = directory.join("publication.json");
        if state::read_optional(root, &publication)?.is_some() {
            return Ok(None);
        }
        mutation.require_absent(publication);
        match work.guard_completion(
            mutation,
            root,
            &prepared.resources,
            &serde_json::to_value(result).into_diagnostic()?,
        ) {
            Ok(()) => {}
            Err(error) if error.downcast_ref::<state::CompletionRefusal>().is_some() => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        for name in ["final.json", "seal.json", "frontier-seal.json"] {
            let path = directory.join(name);
            if let Some(raw) = state::read_optional(root, &path)? {
                mutation.require_unchanged(&path, raw.clone());
                completions.insert(path, raw);
            }
        }
        let final_path = directory.join("final.json");
        let final_raw = completions
            .get(&final_path)
            .ok_or_else(|| miette::miette!("sealed recovery requires its exact final result"))?;
        if lk_core::json::decode_object(final_raw.as_bytes()).into_diagnostic()?
            != serde_json::to_value(result).into_diagnostic()?
        {
            return Err(miette::miette!(
                "recovery proposal differs from its sealed final result"
            ));
        }
    }
    scope.guard_pages(mutation);
    Ok(Some((current, completions)))
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
    let task = work_task_controlled(&root, task_id, &lk_vault::ReadControl::default())?;
    let mut prepared = if task.kind == lk_queue::TaskKind::ExtractConcepts
        && is_progressive(config)?
    {
        let retained = progressive_discovery(&root, &task, discovery)?;
        let Some(queries) = retained.as_deref() else {
            return Ok(json!({"status":"needs-discovery", "context_revision":null,
                "requires_screening":true,"selection":null,"resources":null,
                "authority":"Record explicit discovery queries against the exact target before selecting candidate evidence. No semantic read or completion is certified."}));
        };
        match screening(&root, config, config_revision, task_id, queries)? {
            ScreeningPreparation::Capacity(capacity) => return Err(capacity.into()),
            ScreeningPreparation::SourceBlocked { work_id } => {
                return Err(miette::miette!("{}", source_blocked_reason(&work_id)));
            }
            ScreeningPreparation::Source { work, .. } => {
                return Ok(
                    json!({"status":"needs-source-observations","context_revision":null,
                    "requires_screening":true,"requires_source_work":true,"selection":null,"resources":null,
                    "part_count":work.plan.parts.len(),
                    "plan_command":["lore","work","source-plan","--task-id",task_id]}),
                );
            }
            ScreeningPreparation::Candidates(screening) => {
                if let Some(reason) = screening.selection.admission_reason(config)? {
                    return Ok(json!({"status":"needs-admission", "context_revision":null,
                        "requires_screening":true,"resources":null,"reason":reason,
                        "cost":screening.selection.cost(&screening.inventory.capture)?,
                        "cost_command":["lore","work","cost","--task-id",task_id]}));
                }
                let progress = screening.selection.progress();
                if !progress.complete || !screening.selection.seal()?.is_published(&root)? {
                    return Ok(json!({"status":"needs-screening","context_revision":null,
                        "requires_screening":true,"requires_source_work":false,"resources":null,
                        "progress":progress,
                        "cost":screening.selection.cost(&screening.inventory.capture)?,
                        "plan_command":["lore","work","select","--task-id",task_id],
                        "authority":"A ranked roster is not a semantic assessment, and matches it left out were not examined. Read and record each assigned candidate-card decision; unresolved applicability remains deferred. `work select` seals a completely decided roster before evidence is read. Selected bodies still require full evidence inspection before publication."}));
                }
                let read = lk_vault::ReadControl::default();
                let mut reviews =
                    lk_vault::ReviewInventory::load_controlled(&root, &config.vault.dirs, &read)
                        .into_diagnostic()?;
                admitted_task(&root, config, &screening, &mut reviews, &read)?
            }
        }
    } else {
        match discovery {
            Some(queries) => resources::task_with_discovery(&root, config, task_id, queries)?,
            None => resources::task(&root, config, task_id)?,
        }
    };
    let work = expected(&root, &mut prepared, config, config_revision)?;
    let requires_bounded_work = work.plan.parts.len() > 1 || work.is_planned(&root)?;
    let (prepared, context_revision) = if matches!(
        task.kind,
        lk_queue::TaskKind::ExtractConcepts | lk_queue::TaskKind::SynthesizeConcept
    ) {
        (prepared, work.plan.subject_revision.clone())
    } else {
        let scope =
            resources::section_task_controlled(&root, config, task_id, prepared.read_control())?;
        let revision = scope.revision(config_revision)?;
        (scope, revision)
    };
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
        "selection":prepared.roster.as_ref().map(|roster| &roster.selection),
        "discovery":prepared.roster.as_ref().map(|roster| &roster.coverage),
        "context_revision": context_revision,
        "source_context_revision": work.plan.subject_revision,
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

#[cfg(unix)]
pub(super) fn record_native_discovery(
    session: &lk_vault::MutationSession,
    task: &lk_queue::QueueTask,
    target_revision: String,
    queries: Vec<lk_core::discovery::DiscoveryQuery>,
) -> miette::Result<bool> {
    discovery::record_native(
        session,
        task,
        discovery::DiscoveryRequest {
            target_revision,
            queries,
        },
    )
}

pub(super) fn retire_discovery(
    root: &Path,
    task: &lk_queue::QueueTask,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<()> {
    discovery::retire(root, task, plan)
}

pub(super) fn plan_prune_discovery(
    root: &Path,
    pending: &std::collections::BTreeSet<(String, String)>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    discovery::plan_prune(root, pending, plan)
}

pub(super) fn plan_prune_context_snapshots(
    root: &Path,
    pending: &std::collections::BTreeSet<(String, String)>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    snapshot::plan_prune(root, pending, plan)
}

pub(super) fn plan_prune_screening(
    root: &Path,
    pending: &std::collections::BTreeSet<(String, String)>,
    retained: &std::collections::BTreeSet<(String, String)>,
    changed_queue_files: &std::collections::BTreeSet<PathBuf>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<selection::PreparedScreeningPrune> {
    selection::plan_prune(root, pending, retained, changed_queue_files, plan)
}

pub(super) fn cleanup_screening_directories(
    root: &Path,
    directories: &[PathBuf],
) -> miette::Result<()> {
    selection::cleanup_directories(root, directories)
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
    let mut prepared = resources::task(root, config, &task.task_id)?;
    Ok(expected(root, &mut prepared, config, config_revision)?
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
    let task = work_task_controlled(root, task_id, &lk_vault::ReadControl::default())?;
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
    let screening =
        screening(root, config, config_revision, task_id, &queries)?.into_candidates()?;
    if !screening.selection.progress().complete {
        return Err(miette::miette!(
            "candidate screening is incomplete; use lore work select before planning selected evidence"
        ));
    }
    let read = lk_vault::ReadControl::default();
    let mut reviews = lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, &read)
        .into_diagnostic()?;
    admitted_task(root, config, &screening, &mut reviews, &read)
}

struct Screening {
    selection: selection::Selection,
    inventory: resources::SelectionInventory,
    /// The completed source Work result, when the source was too large to screen whole.
    source_observations: Option<Value>,
}

enum ScreeningPreparation {
    Source {
        work: Box<Work>,
        prepared: Box<Prepared>,
    },
    Candidates(Box<Screening>),
    Capacity(selection::ScreeningCapacity),
    SourceBlocked {
        work_id: String,
    },
}

/// How far a task's source is ready to be screened against.
enum SourceReadiness {
    Ready(Box<selection::Selection>, Option<Value>),
    NeedsObservations(Box<Work>),
    Blocked(String),
}

fn source_blocked_reason(work_id: &str) -> String {
    format!(
        "source observation Work {work_id} retains processing blockers; resolve or replan before candidate screening"
    )
}

/// What a screener reads beside the cards: the task's own content and review scope. It
/// carries no target revision, configuration or Work identity, so a decision stays valid
/// while those change around an unchanged source.
fn source_purpose(source: &Prepared) -> Value {
    let purpose = &source.context["purpose"];
    json!({"task_kind":purpose["task_kind"], "target":purpose["target"],
        "parameters":purpose["parameters"], "review_dependencies":purpose["review_dependencies"]})
}

fn retained_source(source: &Prepared) -> Value {
    json!({"role":"complete-retained-input", "purpose":source_purpose(source),
        "authority":source.context["authority"], "resources":source.resources})
}

fn observed_source(source: &Prepared, observation: &Value) -> Value {
    json!({"role":"complete-bounded-source-observations", "purpose":source_purpose(source),
        "observation":{"output":observation["output"], "evidence":observation["evidence"]}})
}

fn source_characters(source: &Prepared) -> miette::Result<usize> {
    source.resources.iter().try_fold(0usize, |total, resource| {
        total
            .checked_add(resource.text.chars().count())
            .ok_or_else(|| miette::miette!("source reading workload overflow"))
    })
}

fn screening(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
) -> miette::Result<ScreeningPreparation> {
    screening_controlled(
        root,
        config,
        config_revision,
        task_id,
        queries,
        &lk_vault::ReadControl::default(),
    )
}

fn screening_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    queries: &[lk_core::discovery::DiscoveryQuery],
    read: &lk_vault::ReadControl,
) -> miette::Result<ScreeningPreparation> {
    let inventory =
        resources::selection_inventory_controlled(root, config, task_id, queries, read)?;
    screening_with_inventory(root, config, config_revision, inventory)
}

fn screening_with_inventory(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    mut inventory: resources::SelectionInventory,
) -> miette::Result<ScreeningPreparation> {
    let Subject::Queue {
        task_id,
        cache_hash,
        ..
    } = &inventory.source.subject
    else {
        return Err(miette::miette!(
            "candidate screening requires a queue subject"
        ));
    };
    let (task_id, cache_hash) = (task_id.clone(), cache_hash.clone());
    let contract_revision = selection::contract_revision(config)?;
    let identity = selection::SelectionIdentity {
        task_id: &task_id,
        cache_hash: &cache_hash,
        contract_revision: &contract_revision,
    };
    let bounds = limits(config)?;
    let bounds = selection::ScreeningLimits {
        max_input_bytes: bounds.max_input_bytes,
        max_result_bytes: bounds.max_result_bytes,
    };
    let prepared = (|| -> miette::Result<SourceReadiness> {
        let rows = selection::Rows::build(&inventory.capture)?;
        let whole = retained_source(&inventory.source);
        let fits = rows.is_empty()
            || source_characters(&inventory.source)?
                <= lk_core::semantic::INSPECTION_CHARACTERS.get()
                && selection::Selection::source_fits(&identity, &whole, &rows, bounds)?;
        let (source, observations) = if fits {
            (whole, None)
        } else {
            let work = expected(root, &mut inventory.source, config, config_revision)?;
            let observed = if work.is_planned(root)? {
                work.result_if_complete_with_guards(root, &inventory.source.resources)
                    .map_err(|error| match error.downcast_ref::<state::InputCapacity>() {
                        Some(capacity) => selection::ScreeningCapacity::SourceInput {
                            required_bytes: capacity.actual_bytes,
                            max_bytes: capacity.max_bytes,
                        }
                        .into(),
                        None => error,
                    })?
            } else {
                None
            };
            let Some((result, guards)) = observed else {
                return Ok(SourceReadiness::NeedsObservations(Box::new(work)));
            };
            inventory.source.pages.extend(guards);
            if result["output"]["unresolved"]
                .as_array()
                .is_some_and(|values| !values.is_empty())
            {
                return Ok(SourceReadiness::Blocked(work.plan.id));
            }
            (observed_source(&inventory.source, &result), Some(result))
        };
        selection::Selection::build(
            root,
            &identity,
            source,
            &inventory.capture.queries,
            rows,
            bounds,
        )
        .map(|selection| SourceReadiness::Ready(Box::new(selection), observations))
    })();
    Ok(match prepared {
        Ok(SourceReadiness::Ready(selection, source_observations)) => {
            ScreeningPreparation::Candidates(Box::new(Screening {
                selection: *selection,
                inventory,
                source_observations,
            }))
        }
        Ok(SourceReadiness::Blocked(work_id)) => ScreeningPreparation::SourceBlocked { work_id },
        Ok(SourceReadiness::NeedsObservations(work)) => ScreeningPreparation::Source {
            work,
            prepared: Box::new(inventory.source),
        },
        Err(error) => {
            let Some(capacity) = error
                .downcast_ref::<selection::ScreeningCapacity>()
                .copied()
            else {
                return Err(error);
            };
            ScreeningPreparation::Capacity(capacity)
        }
    })
}

fn current_screening(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<ScreeningPreparation> {
    current_screening_controlled(
        root,
        config,
        config_revision,
        task_id,
        &lk_vault::ReadControl::default(),
    )
}

fn current_screening_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<ScreeningPreparation> {
    let task = work_task_controlled(root, task_id, read)?;
    let queries = discovery::current(root, &task)?.ok_or_else(|| {
        miette::miette!(
            "candidate screening requires current explicit discovery; record work discover first"
        )
    })?;
    screening_controlled(root, config, config_revision, task_id, &queries, read)
}

impl ScreeningPreparation {
    fn into_candidates(self) -> miette::Result<Box<Screening>> {
        match self {
            Self::Candidates(screening) => Ok(screening),
            Self::Capacity(capacity) => Err(capacity.into()),
            Self::SourceBlocked { work_id } => {
                Err(miette::miette!("{}", source_blocked_reason(&work_id)))
            }
            Self::Source { .. } => Err(miette::miette!(
                "source observations are incomplete; resume lore work source-plan before candidate screening"
            )),
        }
    }
}

pub(super) struct ScreeningTurn {
    pub screening_id: String,
    pub decided: usize,
    pub candidates: usize,
    pub state: ScreeningState,
}

pub(super) enum ScreeningState {
    Complete,
    Ready(Value),
    Deferred(String),
}

impl Screening {
    /// The next thing this task's screening needs, decided from one captured roster.
    fn turn(&self) -> miette::Result<ScreeningTurn> {
        let progress = self.selection.progress();
        let state = match self.selection.next_request(false)? {
            Some(request) => match self.selection.hold_reason(&request)? {
                Some(reason) => ScreeningState::Deferred(reason),
                None => ScreeningState::Ready(serde_json::to_value(request).into_diagnostic()?),
            },
            None if progress.complete => ScreeningState::Complete,
            None => ScreeningState::Deferred(format!(
                "{} deferred candidate decisions remain; work retained for explicit reassessment",
                progress.deferred
            )),
        };
        Ok(ScreeningTurn {
            state,
            screening_id: progress.screening_id,
            decided: progress.candidates - progress.unknown,
            candidates: progress.candidates,
        })
    }
}

/// One fresh discovery traversal for several tasks, in recipe order, with the review
/// inventory their evidence shares.
struct ScreeningCapture {
    reviews: lk_vault::ReviewInventory,
    captures: Vec<lk_vault::DiscoveryCapture>,
    metrics: lk_vault::DiscoveryCaptureMetrics,
}

impl ScreeningCapture {
    fn load(
        root: &Path,
        config: &lk_core::config::Config,
        recipes: &[lk_vault::DiscoveryRecipe],
        read: &lk_vault::ReadControl,
    ) -> miette::Result<Self> {
        let mut reviews =
            lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
                .into_diagnostic()?;
        let (captures, metrics) = lk_vault::capture_discovery_batch_with_inventory(
            root,
            &config.vault.dirs,
            recipes,
            &mut reviews,
            read,
        )
        .into_diagnostic()?;
        Ok(Self {
            reviews,
            captures,
            metrics,
        })
    }
}

fn current_screening_recipes(
    root: &Path,
    config: &lk_core::config::Config,
    task_ids: &[&str],
) -> miette::Result<Vec<lk_vault::DiscoveryRecipe>> {
    task_ids
        .iter()
        .map(|task_id| {
            let task = work_task_controlled(root, task_id, &lk_vault::ReadControl::default())?;
            let queries = discovery::current(root, &task)?.ok_or_else(|| {
                miette::miette!("candidate screening requires current explicit discovery")
            })?;
            resources::extraction_recipe(config, &task, queries)
        })
        .collect()
}

fn map_current_screenings<T>(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_ids: &[&str],
    mut visit: impl FnMut(miette::Result<Screening>, &mut lk_vault::ReviewInventory) -> T,
    read: &lk_vault::ReadControl,
) -> miette::Result<(Vec<T>, lk_vault::DiscoveryCaptureMetrics, u64)> {
    read.check().into_diagnostic()?;
    let started_at = std::time::Instant::now();
    let recipes = current_screening_recipes(root, config, task_ids)?;
    let mut capture = ScreeningCapture::load(root, config, &recipes, read)?;
    let mut duration_ms = started_at.elapsed().as_millis() as u64;
    let values = task_ids
        .iter()
        .zip(std::mem::take(&mut capture.captures))
        .map(|(task_id, current)| {
            let started_at = std::time::Instant::now();
            let screening = (|| {
                read.check().into_diagnostic()?;
                let inventory = resources::selection_inventory_with_capture(
                    root,
                    config,
                    task_id,
                    current,
                    &mut capture.reviews,
                    read,
                )?;
                screening_with_inventory(root, config, config_revision, inventory)?
                    .into_candidates()
                    .map(|screening| *screening)
            })();
            duration_ms += started_at.elapsed().as_millis() as u64;
            visit(screening, &mut capture.reviews)
        })
        .collect();
    Ok((values, capture.metrics, duration_ms))
}

pub(super) struct ScreeningTurns {
    pub turns: Vec<ScreeningTurn>,
    pub discovery_io: lk_vault::DiscoveryCaptureMetrics,
}

pub(super) fn prepare_screenings(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_ids: &[&str],
    read: &lk_vault::ReadControl,
) -> miette::Result<ScreeningTurns> {
    let root = session.root();
    let (turns, discovery_io, _) = map_current_screenings(
        root,
        config,
        config_revision,
        task_ids,
        |screening, _| {
            let screening = screening?;
            if let Some(reason) = screening.selection.admission_reason(config)? {
                return Err(miette::miette!("{reason}"));
            }
            screening.turn()
        },
        read,
    )?;
    Ok(ScreeningTurns {
        turns: turns.into_iter().collect::<miette::Result<Vec<_>>>()?,
        discovery_io,
    })
}

#[derive(Clone)]
pub(super) enum ScreeningResponse {
    Decisions {
        request: Value,
        output: Value,
    },
    InvocationFailed {
        request: Value,
        failure: state::InvocationFailure,
    },
    Rejected {
        request: Value,
        rejection: ScreeningRejection,
    },
}

pub(super) enum ScreeningRecordOutcome {
    /// The number of candidate decisions this reply recorded.
    Recorded(usize),
    Deferred(String),
    InvocationFailed(String),
}

pub(super) struct ScreeningRecord {
    pub result: miette::Result<ScreeningRecordOutcome>,
    pub duration_ms: u64,
}

/// Record each reply against the exact request it answered. A decision binds its candidate
/// key, so recording checks the pending task and the ledger, never candidate pages.
#[cfg(unix)]
pub(super) fn record_screenings(
    session: &lk_vault::MutationSession,
    replies: Vec<(&str, ScreeningResponse)>,
    read: &lk_vault::ReadControl,
) -> Vec<ScreeningRecord> {
    let root = session.root();
    replies
        .into_iter()
        .map(|(task_id, response)| {
            let started_at = std::time::Instant::now();
            let result = (|| {
                read.check().into_diagnostic()?;
                let task = work_task_controlled(root, task_id, read)?;
                let issued = |request: &Value| {
                    let request = selection::Request::from_value(request)?;
                    if request.task_id() != task.task_id || request.cache_hash() != task.cache_hash
                    {
                        return Err(miette::miette!(
                            "selected screening task changed during execution"
                        ));
                    }
                    Ok(request)
                };
                match response {
                    ScreeningResponse::Decisions { request, output } => {
                        let request = issued(&request)?;
                        let plan = selection::plan_record(
                            root,
                            &request,
                            serde_json::from_value(output).into_diagnostic()?,
                        )?;
                        session.apply(plan).into_diagnostic()?;
                        Ok(ScreeningRecordOutcome::Recorded(request.candidates()))
                    }
                    ScreeningResponse::InvocationFailed { request, failure } => {
                        let reason = failure.to_string();
                        let plan =
                            selection::plan_failed_attempt(root, &issued(&request)?, failure)?;
                        session.apply(plan).into_diagnostic()?;
                        Ok(ScreeningRecordOutcome::InvocationFailed(reason))
                    }
                    ScreeningResponse::Rejected { request, rejection } => {
                        let reason = rejection.to_string();
                        let plan = selection::plan_rejection(root, &issued(&request)?, rejection)?;
                        session.apply(plan).into_diagnostic()?;
                        Ok(ScreeningRecordOutcome::Deferred(reason))
                    }
                }
            })();
            ScreeningRecord {
                result,
                duration_ms: started_at.elapsed().as_millis() as u64,
            }
        })
        .collect()
}

#[cfg(unix)]
pub(super) enum InspectionRecording {
    Recorded(miette::Result<RecordedInspections>),
    Recapture(Vec<InspectionReply>),
    Invalidated(miette::Report),
}

#[cfg(unix)]
pub(super) struct ReadWaveRecords {
    pub screenings: Option<Vec<ScreeningRecord>>,
    pub screening_duration_ms: u64,
    pub inspections: InspectionRecording,
    pub inspection_duration_ms: u64,
}

#[cfg(unix)]
pub(super) fn record_read_wave(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    screenings: Vec<(&str, ScreeningResponse)>,
    inspections: Vec<InspectionReply>,
    validate_context: impl Fn() -> miette::Result<()>,
    read: &lk_vault::ReadControl,
) -> ReadWaveRecords {
    let started = std::time::Instant::now();
    if let Err(error) = validate_context() {
        return ReadWaveRecords {
            screenings: None,
            screening_duration_ms: started.elapsed().as_millis() as u64,
            inspections: InspectionRecording::Invalidated(error),
            inspection_duration_ms: 0,
        };
    }
    let screenings = record_screenings(session, screenings, read);
    let screening_duration_ms = started.elapsed().as_millis() as u64;
    let started = std::time::Instant::now();
    let inspections = if !screenings.iter().all(|record| record.result.is_ok()) {
        InspectionRecording::Recapture(inspections)
    } else if let Err(error) = validate_context() {
        InspectionRecording::Invalidated(error)
    } else {
        InspectionRecording::Recorded(record_inspections(
            session,
            config,
            config_revision,
            inspections,
            read,
        ))
    };
    ReadWaveRecords {
        screenings: Some(screenings),
        screening_duration_ms,
        inspections,
        inspection_duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// The completed source observations of a source too large to read whole, with the Work
/// files they were read from, or nothing while they are not complete.
fn completed_source_observation(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    source: &Prepared,
) -> miette::Result<Option<(Value, std::collections::BTreeMap<PathBuf, String>)>> {
    let mut source = source.clone();
    let work = expected(root, &mut source, config, config_revision)?;
    if !work.is_planned(root)? {
        return Ok(None);
    }
    work.result_if_complete_with_guards(root, &source.resources)
}

/// Evidence joined with the candidate roster a result drafted from it must name.
enum RosterAdmission<'a> {
    Bound(Box<std::borrow::Cow<'a, Prepared>>),
    /// No current roster can be formed (screening capacity, a blocked or incomplete
    /// source), it is not completely decided, or it admits different evidence.
    Changed(String),
}

/// Bind screened evidence to the current candidate roster: one can be formed (screening
/// capacity, source Work and source observations allow it), every row is decided with
/// none deferred, and exactly this evidence is admitted. Other evidence needs no roster.
fn with_current_roster<'a>(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    prepared: &'a Prepared,
    read: &lk_vault::ReadControl,
) -> miette::Result<RosterAdmission<'a>> {
    if !resources::is_screened(&prepared.context) || prepared.roster.is_some() {
        return Ok(RosterAdmission::Bound(Box::new(
            std::borrow::Cow::Borrowed(prepared),
        )));
    }
    let Subject::Queue { task_id, .. } = &prepared.subject else {
        return Err(miette::miette!(
            "screened extraction evidence requires a queue subject"
        ));
    };
    let screening =
        match current_screening_controlled(root, config, config_revision, task_id, read)? {
            ScreeningPreparation::Candidates(screening) => screening,
            ScreeningPreparation::Capacity(capacity) => {
                return Ok(RosterAdmission::Changed(capacity.to_string()));
            }
            ScreeningPreparation::SourceBlocked { work_id } => {
                return Ok(RosterAdmission::Changed(source_blocked_reason(&work_id)));
            }
            ScreeningPreparation::Source { .. } => {
                return Ok(RosterAdmission::Changed(
                    "source observations are incomplete; this extraction cannot finish".into(),
                ));
            }
        };
    let progress = screening.selection.progress();
    if !progress.complete {
        return Ok(RosterAdmission::Changed(format!(
            "candidate screening is incomplete: {} unknown, {} deferred; not-selected covers candidate cards only",
            progress.unknown, progress.deferred,
        )));
    }
    let mut reviews = lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
        .into_diagnostic()?;
    let admitted =
        admitted_task(root, config, &screening, &mut reviews, read)?.read_as(prepared.reading())?;
    if admitted.revision(config_revision)? != prepared.revision(config_revision)? {
        return Ok(RosterAdmission::Changed(
            "candidate screening now admits different evidence than this work read".into(),
        ));
    }
    Ok(RosterAdmission::Bound(Box::new(std::borrow::Cow::Owned(
        admitted,
    ))))
}

/// The same binding after a host returned: a roster that cannot be formed (screening
/// capacity, a blocked or incomplete source), is not completely decided, or admits
/// different evidence is a changed read scope, so the reply is not adopted. Other roster
/// churn rebinds the reply to the current roster.
fn require_current_roster<'a>(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    prepared: &'a Prepared,
    read: &lk_vault::ReadControl,
) -> miette::Result<std::borrow::Cow<'a, Prepared>> {
    match with_current_roster(root, config, config_revision, prepared, read)? {
        RosterAdmission::Bound(prepared) => Ok(*prepared),
        RosterAdmission::Changed(reason) => Err(publication_rejection(
            RejectionKind::SemanticContextChanged,
            format!("{reason}; rescreen and regenerate the result"),
        )),
    }
}

/// Store the completely decided roster a drafter is about to bind, unless it already is.
fn seal_roster(
    root: &Path,
    pin: &super::ConfigurationPin,
    selection: &selection::Selection,
) -> miette::Result<lk_queue::ExtractionSelection> {
    let seal = selection.seal()?;
    if !seal.is_published(root)? {
        let session = super::publication::acquire_mutation_now(root, pin)?;
        let mut plan = lk_vault::MutationPlan::default();
        seal.plan(session.root(), &mut plan)?;
        session.apply(plan).into_diagnostic()?;
    }
    selection.binding()
}

/// The evidence a completely decided roster admits: the source and every selected owner.
fn admitted_task(
    root: &Path,
    config: &lk_core::config::Config,
    screening: &Screening,
    reviews: &mut lk_vault::ReviewInventory,
    read: &lk_vault::ReadControl,
) -> miette::Result<Prepared> {
    let mut prepared = resources::task_with_selected(
        root,
        config,
        &screening.inventory.source,
        &screening.selection.selected_owners(),
        screening.source_observations.as_ref(),
        reviews,
        read,
    )?;
    prepared.roster = Some(
        screening
            .selection
            .roster_binding(&screening.inventory.capture)?,
    );
    Ok(prepared)
}

/// The evidence a staged result was drafted against, with the stored state it was
/// verified through.
struct SealedEvidence {
    prepared: Prepared,
    roster: selection::SealedRoster,
    /// The source-observation Work files read to rebuild an observed source.
    observation_guards: std::collections::BTreeMap<PathBuf, String>,
}

/// Rebuild a staged result's evidence from its sealed roster. The roster's rows are
/// verified through the ledger; only the source and the selected owners are read again.
#[allow(clippy::too_many_arguments)]
fn sealed_task_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &Prepared,
    queries: &[lk_core::discovery::DiscoveryQuery],
    binding: &lk_queue::ExtractionSelection,
    sealing: Option<&selection::RosterSeal>,
    read: &lk_vault::ReadControl,
) -> miette::Result<SealedEvidence> {
    let Subject::Queue {
        task_id,
        cache_hash,
        ..
    } = &task.subject
    else {
        return Err(miette::miette!(
            "a sealed candidate roster requires a queue subject"
        ));
    };
    let sealed = selection::load_sealed(root, task_id, cache_hash, binding, sealing)?;
    if sealed.queries != discovery::validate_queries(queries)? {
        return Err(publication_rejection(
            RejectionKind::EvidenceScopeChanged,
            "the result's discovery queries differ from its sealed candidate roster; regenerate the extraction result",
        ));
    }
    let mut reviews = lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
        .into_diagnostic()?;
    let source =
        resources::extraction_source_controlled(root, config, task_id, &mut reviews, read)?;
    let mut observation_guards = std::collections::BTreeMap::new();
    let revision = |source: &Value| {
        Ok::<_, miette::Report>(lk_core::rss_original::digest(
            &serde_json::to_vec(source).into_diagnostic()?,
        ))
    };
    let observations = if revision(&retained_source(&source))? == sealed.source_revision {
        None
    } else {
        let Some((observation, guards)) =
            completed_source_observation(root, config, config_revision, &source)?.filter(
                |(observation, _)| {
                    revision(&observed_source(&source, observation))
                        .is_ok_and(|current| current == sealed.source_revision)
                },
            )
        else {
            return Err(publication_rejection(
                RejectionKind::SemanticContextChanged,
                "the extraction source changed after its candidates were screened; rescreen and regenerate the result",
            ));
        };
        observation_guards = guards;
        Some(observation)
    };
    let prepared = resources::task_with_selected(
        root,
        config,
        &source,
        &sealed.selected,
        observations.as_ref(),
        &mut reviews,
        read,
    )?;
    Ok(SealedEvidence {
        prepared,
        roster: sealed,
        observation_guards,
    })
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

pub(super) fn guard_audit_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    assessment: &lk_graph::audit::AuditAssessment,
    mutation: &mut lk_vault::MutationPlan,
    read: &lk_vault::ReadControl,
) -> miette::Result<bool> {
    let root = root.canonicalize().into_diagnostic()?;
    let mut prepared = resources::audit_controlled(&root, config, &assessment.path, read)?;
    let work = expected(&root, &mut prepared, config, config_revision)?;
    guard_prepared(
        &root,
        work,
        prepared,
        json!({"assessments":[assessment]}),
        mutation,
    )
}

#[cfg(unix)]
pub(super) fn record_audit_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<Vec<String>> {
    let root = session.root();
    let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
    let directory = Path::new(".lorekeeper/work").join(id);
    let final_path = directory.join("final.json");
    let (final_raw, final_result) = state::load_result(root, &final_path)?
        .ok_or_else(|| miette::miette!("work final result is missing"))?;
    let path = directory.join("assessment.json");
    let (raw, result) = state::load_result(root, &path)?
        .ok_or_else(|| miette::miette!("staged audit assessment is missing"))?;
    if result != final_result {
        return Err(miette::miette!(
            "staged result differs from the verified sealed result; retained for inspection"
        ));
    }
    work.guard_completion(
        &mut lk_vault::MutationPlan::default(),
        root,
        &prepared.resources,
        &result,
    )?;
    let assessment = prepared.validate_audit_result(&result)?;
    super::audit::record_controlled(
        session,
        config,
        config_revision,
        lk_graph::audit::AuditResults {
            assessments: vec![assessment],
        },
        |assessment, mutation| {
            prepared.guard_pages(mutation);
            mutation.require_unchanged(final_path.clone(), final_raw.clone());
            mutation.require_unchanged(path.clone(), raw.clone());
            preparation::plan_retire(
                root,
                &preparation::Key::AuditAssessment {
                    path: assessment.path.clone(),
                },
                mutation,
            )
        },
        read,
    )
}

/// The queue task a Work is planned from and read against: a current task, or a completed
/// extraction the queue still retains, whose result may be reapplied. Planning, every read of a
/// planned resource and publication select their task by this one rule.
pub(super) fn work_task_controlled(
    root: &Path,
    task_id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<lk_queue::QueueTask> {
    super::queue::select_task_controlled(
        root,
        task_id,
        super::queue::TaskSelection::CurrentOrDoneExtraction,
        read,
    )
}

fn load(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<(Work, Prepared)> {
    load_controlled(
        root,
        config,
        config_revision,
        id,
        &lk_vault::ReadControl::default(),
    )
}

fn load_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<(Work, Prepared)> {
    read.check().into_diagnostic()?;
    let work = Work::load(root, id)?;
    let prepared =
        resources::refresh_controlled(root, config, config_revision, &work.context, read)?;
    validate_loaded_work(root, config, config_revision, work, prepared)
}

fn validate_loaded_work(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    work: Work,
    mut prepared: Prepared,
) -> miette::Result<(Work, Prepared)> {
    if expected(root, &mut prepared, config, config_revision)? != work {
        return Err(publication_rejection(
            RejectionKind::SemanticContextChanged,
            "work inputs, configuration or bounded execution policy changed; replan",
        ));
    }
    Ok((work, prepared))
}

pub(super) enum TaskWork {
    WholeInput {
        #[cfg(unix)]
        native: Option<Box<Value>>,
        #[cfg(unix)]
        individual: bool,
    },
    Partitioned {
        id: String,
    },
    #[cfg(unix)]
    Finalization {
        id: String,
    },
    #[cfg(unix)]
    RootFinalization {
        id: String,
    },
    Source {
        id: String,
    },
    Screening {
        turn: Box<ScreeningTurn>,
    },
    Admission {
        reason: String,
    },
}

pub(super) struct TaskPreparation {
    pub work: TaskWork,
    #[cfg(unix)]
    pub admissions: Vec<native::FinalizationAdmission>,
}

impl TaskPreparation {
    fn from_result(result: miette::Result<Self>) -> miette::Result<Self> {
        match result {
            Err(error) if state::is_input_capacity(&error) || is_citations_changed(&error) => {
                Ok(TaskWork::Admission {
                    reason: error.to_string(),
                }
                .into())
            }
            result => result,
        }
    }
}

impl From<TaskWork> for TaskPreparation {
    fn from(work: TaskWork) -> Self {
        Self {
            work,
            #[cfg(unix)]
            admissions: Vec::new(),
        }
    }
}

#[cfg(unix)]
fn validate_native_finalization(
    root: &Path,
    config: &lk_core::config::Config,
    work: &Work,
    prepared: &Prepared,
    request: &Value,
) -> miette::Result<Value> {
    validate_result_finalization(
        root,
        config,
        work,
        prepared,
        request,
        state::CompletionKind::Root,
    )
}

#[cfg(unix)]
fn validate_result_finalization(
    root: &Path,
    config: &lk_core::config::Config,
    work: &Work,
    prepared: &Prepared,
    request: &Value,
    kind: state::CompletionKind,
) -> miette::Result<Value> {
    current_result_finalization(root, config, work, prepared, request, kind)?
        .map(|snapshot| snapshot.result().clone())
        .ok_or_else(|| {
            publication_rejection(
                RejectionKind::SemanticContextChanged,
                "native finalization scope or completed observations changed; reread and regenerate",
            )
        })
}

/// The completed Work whose current finalizer request is still `request`, or `None` when its
/// scope or completed observations changed after the request was prepared.
#[cfg(unix)]
fn current_result_finalization(
    root: &Path,
    config: &lk_core::config::Config,
    work: &Work,
    prepared: &Prepared,
    request: &Value,
    kind: state::CompletionKind,
) -> miette::Result<Option<state::CompletedWork>> {
    let snapshot = work.capture_native_completion(root, &prepared.resources, kind)?;
    let completed = snapshot.result();
    let current = match &prepared.subject {
        Subject::Audit { .. } if kind == state::CompletionKind::Root => {
            audit::request(work, prepared, completed)?
        }
        Subject::Audit { .. } => {
            return Err(miette::miette!(
                "audit finalization requires root completion"
            ));
        }
        Subject::Queue { task_id, .. } => {
            let task = super::queue::current_task(root, task_id)?;
            match kind {
                state::CompletionKind::Root => native::finalization_request(
                    config,
                    &task,
                    work,
                    &generation::Owners::build(config, prepared)?,
                    prepared.roster.as_ref(),
                    &work.plan.subject_revision,
                    completed,
                )?,
                state::CompletionKind::Frontier => native::frontier_request(
                    config,
                    &task,
                    work,
                    &generation::Owners::build(config, prepared)?,
                    prepared.roster.as_ref(),
                    completed,
                )?,
            }
        }
    };
    if &current != request {
        return Ok(None);
    }
    if serde_json::to_vec(&current).into_diagnostic()?.len() > work.max_input_bytes {
        return Err(miette::miette!(
            "complete native finalizer exceeds max_input_bytes"
        ));
    }
    Ok(Some(snapshot))
}

#[cfg(unix)]
fn load_native_finalization_input(
    explicit: Option<&Path>,
    inherited: Option<(&Path, &str)>,
) -> miette::Result<Option<Value>> {
    let path = inherited.map(|(path, _)| path).or(explicit);
    let Some(path) = path else {
        return Ok(None);
    };
    let bytes = std::fs::read(path).into_diagnostic()?;
    if let Some((_, revision)) = inherited {
        if lk_core::rss_original::digest(&bytes) != revision {
            return Err(miette::miette!(
                "native finalizer handoff bytes changed; retained work requires a fresh invocation"
            ));
        }
        if let Some(explicit) = explicit
            && std::fs::read(explicit).into_diagnostic()? != bytes
        {
            return Err(miette::miette!(
                "prepared input differs from the native finalizer handoff"
            ));
        }
    }
    lk_core::json::decode_object(&bytes)
        .map(Some)
        .into_diagnostic()
}

#[cfg(unix)]
fn finish_with_preparation(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    work: &Work,
    prepared: &Prepared,
    result: Value,
    request: Option<&Value>,
) -> miette::Result<Value> {
    if let Some(request) = request {
        let completed =
            validate_native_finalization(session.root(), config, work, prepared, request)?;
        let (path, mutation) =
            plan_finish_expected(session, work, prepared, result, Some(&completed))?;
        session.apply(mutation).into_diagnostic()?;
        Ok(json!({"work_id":work.plan.id,"staged_result":path,"canonical_published":false}))
    } else {
        finish(session, work, prepared, result)
    }
}

#[cfg(test)]
pub(super) fn prepare_task(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
) -> miette::Result<TaskPreparation> {
    let read = lk_vault::ReadControl::default();
    let task = super::queue::current_task_controlled(session.root(), task_id, &read)?;
    prepare_task_controlled(session, config, config_revision, &task, &read)
}

/// Prepare a task this checkpoint already selected as current.
pub(super) fn prepare_task_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
    read: &lk_vault::ReadControl,
) -> miette::Result<TaskPreparation> {
    TaskPreparation::from_result(prepare_task_in_session(
        session,
        config,
        config_revision,
        task,
        None,
        None,
        read,
    ))
}

pub(super) fn prepare_tasks_with_audit(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    tasks: &[lk_queue::QueueTask],
    audits: &std::collections::BTreeMap<String, lk_graph::audit::AuditCandidate>,
    read: &lk_vault::ReadControl,
) -> miette::Result<(Vec<TaskPreparation>, lk_vault::DiscoveryCaptureMetrics)> {
    read.check().into_diagnostic()?;
    let root = session.root();
    let mut recipes = Vec::new();
    let mut positions = Vec::new();
    if is_progressive(config)? {
        for (index, task) in tasks.iter().enumerate() {
            if task.kind == lk_queue::TaskKind::ExtractConcepts
                && let Some(queries) = discovery::current(root, task)?
            {
                positions.push(index);
                recipes.push(resources::extraction_recipe(config, task, queries)?);
            }
        }
    }
    let mut reviews = if recipes.is_empty() {
        None
    } else {
        Some(
            lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
                .into_diagnostic()?,
        )
    };
    let (captures, metrics) = match reviews.as_mut() {
        Some(reviews) => lk_vault::capture_discovery_batch_with_inventory(
            root,
            &config.vault.dirs,
            &recipes,
            reviews,
            read,
        )
        .into_diagnostic()?,
        None => (Vec::new(), lk_vault::DiscoveryCaptureMetrics::default()),
    };
    let mut captures = positions.into_iter().zip(captures).peekable();
    let pending = super::queue::PendingTasks::load_controlled(root, read)?;
    let work = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            read.check().into_diagnostic()?;
            let task = pending.current_controlled(root, &task.task_id, read)?;
            let screening = if captures.peek().is_some_and(|(at, _)| *at == index) {
                let (_, capture) = captures.next().expect("indexed capture");
                let reviews = reviews
                    .as_mut()
                    .expect("discovery capture has a review inventory");
                let inventory = resources::selection_inventory_with_capture(
                    root,
                    config,
                    &task.task_id,
                    capture,
                    reviews,
                    read,
                )?;
                Some((inventory, reviews))
            } else {
                None
            };
            TaskPreparation::from_result(prepare_task_in_session(
                session,
                config,
                config_revision,
                &task,
                audits.get(&task.target.vault_path),
                screening,
                read,
            ))
        })
        .collect::<miette::Result<Vec<_>>>()?;
    read.check().into_diagnostic()?;
    Ok((work, metrics))
}

fn prepare_task_in_session(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
    audit: Option<&lk_graph::audit::AuditCandidate>,
    screening: Option<(
        resources::SelectionInventory,
        &mut lk_vault::ReviewInventory,
    )>,
    read: &lk_vault::ReadControl,
) -> miette::Result<TaskPreparation> {
    let root = session.root();
    let mut prepared = if task.kind == lk_queue::TaskKind::ExtractConcepts
        && is_progressive(config)?
        && discovery::current(root, task)?.is_some()
    {
        let mut own_reviews;
        let (selected, reviews) = match screening {
            Some((inventory, reviews)) => (
                screening_with_inventory(root, config, config_revision, inventory)?,
                reviews,
            ),
            None => {
                own_reviews =
                    lk_vault::ReviewInventory::load_controlled(root, &config.vault.dirs, read)
                        .into_diagnostic()?;
                (
                    current_screening_controlled(
                        root,
                        config,
                        config_revision,
                        &task.task_id,
                        read,
                    )?,
                    &mut own_reviews,
                )
            }
        };
        match selected {
            ScreeningPreparation::SourceBlocked { work_id } => {
                return Ok(TaskWork::Admission {
                    reason: source_blocked_reason(&work_id),
                }
                .into());
            }
            ScreeningPreparation::Capacity(capacity) => {
                return Ok(TaskWork::Admission {
                    reason: capacity.to_string(),
                }
                .into());
            }
            ScreeningPreparation::Source { work, mut prepared } => {
                persist_prepared(session, config, config_revision, &mut prepared)?;
                if let Some(reason) = inspection_admission(root, &work, &prepared.resources)? {
                    return Ok(TaskWork::Admission { reason }.into());
                }
                return Ok(TaskWork::Source { id: work.plan.id }.into());
            }
            ScreeningPreparation::Candidates(screening) => {
                if let Some(reason) = screening.selection.admission_reason(config)? {
                    return Ok(TaskWork::Admission { reason }.into());
                }
                let turn = screening.turn()?;
                match turn.state {
                    ScreeningState::Complete => {
                        admitted_task(root, config, &screening, reviews, read)?
                    }
                    ScreeningState::Deferred(reason) => {
                        return Ok(TaskWork::Admission { reason }.into());
                    }
                    ScreeningState::Ready(_) => {
                        return Ok(TaskWork::Screening {
                            turn: Box::new(turn),
                        }
                        .into());
                    }
                }
            }
        }
    } else {
        resources::task_with_audit_controlled(root, config, &task.task_id, audit, read)?
    };
    let work = expected(root, &mut prepared, config, config_revision)?;
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
        #[cfg(not(unix))]
        {
            work.whole_input_bytes(&prepared.resources)?;
            return Ok(TaskWork::WholeInput {}.into());
        }
        #[cfg(unix)]
        {
            if super::execution::finalization::supports(task.kind) {
                work.whole_input_bytes(&prepared.resources)?;
                return Ok(TaskWork::WholeInput {
                    native: None,
                    individual: false,
                }
                .into());
            }
            let owner = lk_vault::resolve_page_owner(root, Path::new(&task.target.vault_path))
                .into_diagnostic()?;
            let input = match native::selected_task(config, task, &prepared, &work, &owner) {
                Ok(input) => input,
                Err(error) if error.downcast_ref::<generation::ResultCapacity>().is_some() => {
                    return Ok(TaskWork::Admission {
                        reason: error.to_string(),
                    }
                    .into());
                }
                Err(error) => return Err(error),
            };
            let individual = match generation::admission(root, config, &input)? {
                generation::Admission::Batch => false,
                generation::Admission::Single => true,
                generation::Admission::Deferred(reason) => {
                    return Ok(TaskWork::Admission { reason }.into());
                }
            };
            let request = native::selected_request(config, std::slice::from_ref(&input))?;
            let bytes = serde_json::to_vec(&request).into_diagnostic()?.len();
            if bytes <= work.max_input_bytes
                && native::selected_characters(std::slice::from_ref(&input))?
                    <= lk_core::semantic::INSPECTION_CHARACTERS.get()
            {
                return Ok(TaskWork::WholeInput {
                    native: Some(Box::new(input)),
                    individual,
                }
                .into());
            }
        }
    }
    persist_prepared(session, config, config_revision, &mut prepared)?;
    let mut preparation = TaskPreparation::from(TaskWork::Partitioned {
        id: work.plan.id.clone(),
    });
    #[cfg(unix)]
    {
        use native::{FinalizationAdmission, FinalizationAdmissionState, FinalizationMode};

        if work.has_root_output(root)? && work.progress(root, &prepared.resources)?.complete {
            let checkpoint = prepare_native_finalization_from_capture(
                root,
                config,
                config_revision,
                &work,
                &prepared,
                false,
            )?;
            let outcome = match &checkpoint.state {
                NativeFinalization::Finished => FinalizationAdmissionState::Finished,
                NativeFinalization::Replan => FinalizationAdmissionState::Replan,
                NativeFinalization::Ready(FinalizationPreparation::NeedsAction(_)) => {
                    FinalizationAdmissionState::NeedsAction
                }
                NativeFinalization::Ready(FinalizationPreparation::Capacity {
                    request_bytes,
                    max_input_bytes,
                }) => FinalizationAdmissionState::PreparedInputCapacity {
                    request_bytes: *request_bytes,
                    max_input_bytes: *max_input_bytes,
                },
                NativeFinalization::Ready(_) => FinalizationAdmissionState::Available {
                    request_bytes: None,
                },
            };
            let mut admission =
                FinalizationAdmission::new(&work, task, FinalizationMode::Root, outcome);
            if let NativeFinalization::Ready(FinalizationPreparation::Result(result)) =
                &checkpoint.state
            {
                admission = admission.with_completion(&result.completed)?;
            }
            preparation.admissions.push(admission);
            match checkpoint.state {
                NativeFinalization::Ready(FinalizationPreparation::NeedsAction(reason)) => {
                    preparation.work = TaskWork::Admission { reason };
                    return Ok(preparation);
                }
                NativeFinalization::Ready(FinalizationPreparation::Capacity {
                    request_bytes,
                    max_input_bytes,
                }) => {
                    preparation.work = TaskWork::Admission {
                        reason: finalization_capacity_reason(request_bytes, max_input_bytes),
                    };
                    return Ok(preparation);
                }
                NativeFinalization::Finished | NativeFinalization::Ready(_) => {
                    preparation.work = TaskWork::RootFinalization { id: work.plan.id };
                    return Ok(preparation);
                }
                NativeFinalization::Replan => {}
            }
        }
        let seal = PathBuf::from(".lorekeeper/work")
            .join(&work.plan.id)
            .join("frontier-seal.json");
        if state::read_optional(root, &seal)?.is_some() {
            match verify_finished_prepared(root, &work, &prepared) {
                Ok(()) => {
                    preparation.admissions.push(FinalizationAdmission::new(
                        &work,
                        task,
                        FinalizationMode::Frontier,
                        FinalizationAdmissionState::ReusableSeal,
                    ));
                    preparation.work = TaskWork::Finalization { id: work.plan.id };
                    return Ok(preparation);
                }
                Err(error)
                    if matches!(
                        error.downcast_ref::<state::CompletionRefusal>(),
                        Some(state::CompletionRefusal::ChangedObservations)
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        match frontier_result(root, config, task, &work, &prepared)? {
            FrontierFinalization::Unavailable(admission) => {
                preparation.admissions.push(admission);
            }
            FrontierFinalization::Ready { result, admission } => {
                let hold = native_finalization_hold(
                    root,
                    task,
                    &work,
                    &result.request,
                    &result.completed,
                )?;
                let admission = match &hold {
                    None => admission,
                    Some(FinalizationHold::DiscoveryRecovery(_)) => {
                        admission.with_outcome(FinalizationAdmissionState::DiscoveryRecovery)
                    }
                    Some(FinalizationHold::ProcessingAction(_)) => {
                        admission.with_outcome(FinalizationAdmissionState::ProcessingAction)
                    }
                    Some(FinalizationHold::GenerationRejection(_)) => {
                        admission.with_outcome(FinalizationAdmissionState::GenerationRejected)
                    }
                };
                preparation.admissions.push(admission);
                match hold {
                    None => {
                        preparation.work = TaskWork::Finalization { id: work.plan.id };
                        return Ok(preparation);
                    }
                    Some(
                        FinalizationHold::DiscoveryRecovery(reason)
                        | FinalizationHold::GenerationRejection(reason),
                    ) => {
                        preparation.work = TaskWork::Admission { reason };
                        return Ok(preparation);
                    }
                    Some(FinalizationHold::ProcessingAction(_)) => {}
                }
            }
        }
    }
    if let Some(reason) = inspection_admission(root, &work, &prepared.resources)? {
        preparation.work = TaskWork::Admission { reason };
    }
    Ok(preparation)
}

fn inspection_admission(
    root: &Path,
    work: &Work,
    resources: &[lk_core::semantic::SemanticResource],
) -> miette::Result<Option<String>> {
    match work.prepare_inspection(root, resources) {
        #[cfg(unix)]
        Ok((_, Some(request))) => work.load_inspection_hold(
            root,
            &request,
            &super::execution::observation::contract_revision(&request)?,
        ),
        Ok(_) => Ok(None),
        Err(error) if error.downcast_ref::<state::InputCapacity>().is_some() => {
            Ok(Some(error.to_string()))
        }
        Err(error) => Err(error),
    }
}

pub(super) fn prepare_audit_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    read: &lk_vault::ReadControl,
) -> miette::Result<String> {
    #[cfg(unix)]
    audit::validate_result_capacity(candidate, limits(config)?.max_result_bytes)?;
    let root = session.root();
    let mut prepared = resources::audit_candidate_controlled(root, config, candidate, read)?;
    let work = persist_prepared(session, config, config_revision, &mut prepared)?;
    Ok(work.plan.id)
}

pub(super) fn audit_needs_work_controlled(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    read: &lk_vault::ReadControl,
) -> miette::Result<bool> {
    let root = session.root();
    let mut prepared = resources::audit_candidate_controlled(root, config, candidate, read)?;
    let work = expected(root, &mut prepared, config, config_revision)?;
    if work.plan.parts.len() > 1 || work.is_planned(root)? {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        let subject = audit::capture_prepared(root, config_revision, candidate, prepared, &work)?;
        let request = audit::whole_request(config, &[subject]);
        match request {
            Ok(request) => {
                Ok(serde_json::to_vec(&request).into_diagnostic()?.len() > work.max_input_bytes)
            }
            Err(error) if error.downcast_ref::<generation::ResultCapacity>().is_some() => Ok(true),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(unix))]
    Ok(false)
}

#[cfg(unix)]
pub(super) enum AuditProcessingHold {
    Capacity(String),
    Action(String),
}

#[cfg(unix)]
impl AuditProcessingHold {
    pub fn into_reason(self) -> String {
        match self {
            Self::Capacity(reason) | Self::Action(reason) => reason,
        }
    }
}

#[cfg(unix)]
pub(super) fn audit_processing_hold_controlled(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    read: &lk_vault::ReadControl,
) -> miette::Result<Option<AuditProcessingHold>> {
    if let Err(error) = audit::validate_result_capacity(candidate, limits(config)?.max_result_bytes)
    {
        if error.downcast_ref::<generation::ResultCapacity>().is_some() {
            return Ok(Some(AuditProcessingHold::Capacity(error.to_string())));
        }
        return Err(error);
    }
    if candidate.discovery.is_none() {
        let binding = preparation::Binding::audit_discovery(
            &candidate.path,
            &candidate.file_revision,
            &json!({}),
            &json!({}),
        )?;
        return Ok(binding.held(root)?.map(AuditProcessingHold::Action));
    }
    if let Some(progress) = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
        && !progress.is_ready()
    {
        let budget = config.execution.effective_work().into_diagnostic()?;
        if progress.unknown != 0 {
            let packet = match lk_graph::audit::selection::prepare_candidate_packet(
                root,
                &config.vault.dirs,
                candidate,
                lk_graph::audit::selection::ScreeningLimits {
                    max_input_bytes: budget.max_input_bytes,
                    max_result_bytes: budget.max_result_bytes,
                    max_candidates: 20,
                    max_batches: budget.max_screening_batches.map(|value| value.get()),
                },
                false,
                read,
            ) {
                Ok(packet) => packet,
                Err(error @ lk_graph::GraphError::AuditScreeningCapacity { .. }) => {
                    return Ok(Some(AuditProcessingHold::Capacity(error.to_string())));
                }
                Err(error) => return Err(miette::miette!("{error}")),
            };
            if let Some(admission) = packet.admission {
                return Ok(Some(AuditProcessingHold::Capacity(format!(
                    "complete audit discovery needs {} packets, above max_screening_batches {}; all candidates remain retained",
                    admission.required_batches, admission.maximum_batches
                ))));
            }
            if let Some(request) = packet.request {
                let native = super::execution::preparation::screening_request(
                    &request,
                    &packet.request_revision.expect("prepared packet revision"),
                )?;
                let binding = preparation::Binding::audit_screening(
                    &request,
                    &native,
                    &super::execution::preparation::screening_schema(),
                )?;
                if let Some(reason) = binding.held(root)? {
                    return Ok(Some(AuditProcessingHold::Action(reason)));
                }
            }
        }
        return Ok(None);
    }
    let mut prepared = resources::audit_candidate_controlled(root, config, candidate, read)?;
    let work = match expected(root, &mut prepared, config, config_revision) {
        Ok(work) => work,
        Err(error) if state::is_input_capacity(&error) => {
            return Ok(Some(AuditProcessingHold::Capacity(error.to_string())));
        }
        Err(error) => return Err(error),
    };
    let planned = work.is_planned(root)?;
    if planned {
        match is_finished_prepared(root, config, config_revision, &work, &prepared) {
            Ok(true) => return Ok(None),
            Ok(false) => {}
            Err(error) => {
                return match super::audit::assessment_blocker(&error) {
                    Some(reason) => Ok(Some(AuditProcessingHold::Action(reason))),
                    None => Err(error),
                };
            }
        }
    }
    if let Some(reason) = audit::processing_hold(root, config_revision, candidate, &prepared)? {
        return Ok(Some(AuditProcessingHold::Action(reason)));
    }
    if !planned {
        return Ok(None);
    }
    let (progress, hold) = match work.prepare_inspection_admission(
        root,
        &prepared.resources,
        super::execution::observation::contract_revision,
    ) {
        Ok(admission) => admission,
        Err(error) if state::is_input_capacity(&error) => {
            return Ok(Some(AuditProcessingHold::Capacity(error.to_string())));
        }
        Err(error) => return Err(error),
    };
    if hold.is_some() || !progress.complete {
        return Ok(hold.map(AuditProcessingHold::Action));
    }
    match prepare_finalization_from_capture(root, config, config_revision, &work, &prepared)? {
        FinalizationPreparation::Capacity {
            request_bytes,
            max_input_bytes,
        } => Ok(Some(AuditProcessingHold::Capacity(
            finalization_capacity_reason(request_bytes, max_input_bytes),
        ))),
        FinalizationPreparation::Result(_) => Ok(None),
        FinalizationPreparation::NeedsAction(reason) => {
            Ok(Some(AuditProcessingHold::Action(reason)))
        }
        FinalizationPreparation::Ready(_) => Err(miette::miette!(
            "audit admission produced a queue finalization state"
        )),
    }
}

#[cfg(unix)]
pub(super) enum NativeStep {
    Inspection {
        progress: state::WorkProgress,
        request: Option<Value>,
        preparation_duration_ms: u64,
    },
    Finalization {
        root_node: String,
        checkpoint: FinalizationCheckpoint,
    },
}

#[cfg(unix)]
pub(super) fn prepare_native_step(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<NativeStep> {
    let started_at = std::time::Instant::now();
    let root = session.root();
    let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
    let (progress, request) = work.prepare_inspection(root, &prepared.resources)?;
    let preparation_duration_ms = started_at.elapsed().as_millis() as u64;
    read.check().into_diagnostic()?;
    if !progress.complete {
        return Ok(NativeStep::Inspection {
            progress,
            request,
            preparation_duration_ms,
        });
    }
    let mut checkpoint = prepare_native_finalization_from_capture(
        root,
        config,
        config_revision,
        &work,
        &prepared,
        false,
    )?;
    checkpoint.preparation_duration_ms += preparation_duration_ms;
    read.check().into_diagnostic()?;
    Ok(NativeStep::Finalization {
        root_node: progress.root_node,
        checkpoint,
    })
}

#[cfg(unix)]
pub(super) fn validate_inspection_request(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    node: &str,
    request: &Value,
    read: &lk_vault::ReadControl,
) -> miette::Result<()> {
    let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
    if work.input(root, node, &prepared.resources)? != *request {
        return Err(miette::miette!(
            "inspection inputs changed while waiting for writer access; unfinished work retained"
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn inspection_action(
    root: &Path,
    id: &str,
    request: &Value,
) -> miette::Result<Option<String>> {
    state::Work::load(root, id)?.load_inspection_hold(
        root,
        request,
        &super::execution::observation::contract_revision(request)?,
    )
}

#[cfg(unix)]
pub(super) enum InspectionResponse {
    InvocationFailed(state::InvocationFailure),
    Complete(Box<state::ObservationCandidate>),
    NeedsAction(String),
    SmallerInspection {
        reason: String,
        response: state::RefinementResponse,
    },
    Capacity(state::OutputCapacity),
}

#[cfg(unix)]
pub(super) struct InspectionReply {
    pub work_id: String,
    pub node_id: String,
    pub request: Value,
    pub response: InspectionResponse,
}

#[cfg(unix)]
pub(super) enum InspectionRecordOutcome {
    InvocationFailed(String),
    Recorded(state::WorkProgress),
    Refined { added_nodes: usize },
    Deferred(String),
    Rejected(String),
    InvalidText(String),
    InvalidEvidence(String),
}

#[cfg(unix)]
pub(super) struct InspectionRecord {
    pub result: miette::Result<InspectionRecordOutcome>,
    pub duration_ms: u64,
}

#[cfg(unix)]
pub(super) struct RecordedInspections {
    pub records: Vec<InspectionRecord>,
    pub revalidation_duration_ms: u64,
}

#[cfg(unix)]
struct InspectionWorks {
    works: std::collections::BTreeMap<String, miette::Result<(Work, Prepared)>>,
    duration_ms: u64,
}

#[cfg(unix)]
fn load_inspection_works(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    ids: &[&str],
    read: &lk_vault::ReadControl,
) -> miette::Result<InspectionWorks> {
    let started = std::time::Instant::now();
    let mut works = std::collections::BTreeMap::new();
    for id in ids {
        read.check().into_diagnostic()?;
        if works.contains_key(*id) {
            continue;
        }
        let loaded = Work::load(root, id).and_then(|work| {
            let prepared =
                resources::refresh_controlled(root, config, config_revision, &work.context, read)?;
            validate_loaded_work(root, config, config_revision, work, prepared)
        });
        works.insert((*id).to_owned(), loaded);
    }
    Ok(InspectionWorks {
        works,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// A Work's fresh validation failure as each of its replies reports it. A typed refusal keeps
/// its kind, so a reply whose inputs changed while its host ran reports changed inputs.
#[cfg(unix)]
fn fresh_validation_failure(error: &miette::Report) -> miette::Report {
    if let Some(refusal) = error.downcast_ref::<super::queue::application::PublicationRejection>() {
        return publication_rejection(refusal.kind, refusal.detail.clone());
    }
    if let Some(gone) = error.downcast_ref::<super::queue::TaskNotCurrent>() {
        return miette::Report::new(gone.clone());
    }
    if let Some(held) = error.downcast_ref::<super::queue::SynthesisHeld>() {
        return miette::Report::new(held.clone());
    }
    miette::miette!("fresh work validation failed: {error:#}")
}

#[cfg(unix)]
pub(super) struct PreparedInspections {
    pub turns: Vec<miette::Result<(state::WorkProgress, Vec<Value>)>>,
    pub duration_ms: u64,
}

#[cfg(unix)]
pub(super) fn prepare_inspections_batch(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    demands: &[(String, usize)],
    read: &lk_vault::ReadControl,
) -> miette::Result<PreparedInspections> {
    let started = std::time::Instant::now();
    let mut ids = std::collections::BTreeSet::new();
    if demands
        .iter()
        .any(|(id, limit)| *limit == 0 || !ids.insert(id.as_str()))
    {
        return Err(miette::miette!(
            "inspection demands require unique work and positive limits"
        ));
    }
    let root = session.root();
    let mut loaded = load_inspection_works(
        root,
        config,
        config_revision,
        &ids.into_iter().collect::<Vec<_>>(),
        read,
    )?;
    let turns = demands
        .iter()
        .map(|(id, limit)| {
            read.check().into_diagnostic()?;
            let (work, prepared) = loaded.works.remove(id).expect("selected work")?;
            work.prepare_inspections(root, &prepared.resources, *limit)
        })
        .collect();
    Ok(PreparedInspections {
        turns,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(unix)]
pub(super) fn record_inspections(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    replies: Vec<InspectionReply>,
    read: &lk_vault::ReadControl,
) -> miette::Result<RecordedInspections> {
    let root = session.root();
    let ids = replies
        .iter()
        .map(|reply| reply.work_id.as_str())
        .collect::<Vec<_>>();
    let loaded = load_inspection_works(root, config, config_revision, &ids, read)?;
    record_prepared_inspections(session, loaded, replies, read)
}

#[cfg(unix)]
fn record_prepared_inspections(
    session: &lk_vault::MutationSession,
    loaded: InspectionWorks,
    replies: Vec<InspectionReply>,
    read: &lk_vault::ReadControl,
) -> miette::Result<RecordedInspections> {
    let root = session.root();
    let mut selected = std::collections::BTreeSet::new();
    let records = replies
        .into_iter()
        .map(|reply| {
            let started = std::time::Instant::now();
            let result = (|| {
                read.check().into_diagnostic()?;
                if !selected.insert((reply.work_id.clone(), reply.node_id.clone())) {
                    return Err(miette::miette!(
                        "duplicate inspection reply for a selected node"
                    ));
                }
                let (work, prepared) = loaded.works[&reply.work_id]
                    .as_ref()
                    .map_err(fresh_validation_failure)?;
                match reply.response {
                    InspectionResponse::Complete(candidate) => {
                        if candidate.output.work_id != reply.work_id
                            || candidate.output.node_id != reply.node_id
                        {
                            return Err(miette::miette!(
                                "observation does not answer the selected work node"
                            ));
                        }
                        let contract =
                            super::execution::observation::contract_revision(&reply.request)?;
                        match work.plan_native_observation(
                            root,
                            &prepared.resources,
                            &reply.request,
                            &contract,
                            *candidate,
                        )? {
                            state::ObservationRecordPlan::Accepted(mut mutation) => {
                                prepared.guard_pages(&mut mutation);
                                session.apply(mutation).into_diagnostic()?;
                                work.progress(root, &prepared.resources)
                                    .map(InspectionRecordOutcome::Recorded)
                            }
                            state::ObservationRecordPlan::Rejected {
                                mut mutation,
                                violation,
                            } => {
                                let reason = format!("{violation}; no observation was recorded");
                                prepared.guard_pages(&mut mutation);
                                session.apply(mutation).into_diagnostic()?;
                                Ok(match violation {
                                    state::ObservationViolation::Text(_) => {
                                        InspectionRecordOutcome::InvalidText(reason)
                                    }
                                    state::ObservationViolation::Evidence(_) => {
                                        InspectionRecordOutcome::InvalidEvidence(reason)
                                    }
                                })
                            }
                        }
                    }
                    response => {
                        let revision =
                            super::execution::observation::contract_revision(&reply.request)?;
                        let (mut plan, outcome) = match response {
                            InspectionResponse::InvocationFailed(failure) => {
                                let (plan, held) = work.plan_inspection_attempt(
                                    root,
                                    &prepared.resources,
                                    &reply.request,
                                    &revision,
                                    state::AttemptUpdate::Failed(failure),
                                )?;
                                let reason = held.expect("a recorded failure holds its request");
                                (plan, InspectionRecordOutcome::InvocationFailed(reason))
                            }
                            InspectionResponse::SmallerInspection { reason, response } => {
                                match work.plan_native_refinement(
                                    root,
                                    &prepared.resources,
                                    &reply.request,
                                    &revision,
                                    reason.clone(),
                                    response.clone(),
                                ) {
                                    Ok((plan, preview)) => (
                                        plan,
                                        InspectionRecordOutcome::Refined {
                                            added_nodes: preview.added_nodes(),
                                        },
                                    ),
                                    Err(error) => {
                                        let Some(capacity) =
                                            error.downcast_ref::<state::InputCapacity>()
                                        else {
                                            return Err(error);
                                        };
                                        (
                                            work.plan_inspection_refinement_hold(
                                                root,
                                                &prepared.resources,
                                                &reply.request,
                                                &revision,
                                                reason,
                                                response,
                                            )?,
                                            InspectionRecordOutcome::Rejected(capacity.to_string()),
                                        )
                                    }
                                }
                            }
                            InspectionResponse::NeedsAction(reason) => (
                                work.plan_inspection_action(
                                    root,
                                    &prepared.resources,
                                    &reply.request,
                                    &revision,
                                    reason.clone(),
                                )?,
                                InspectionRecordOutcome::Deferred(reason),
                            ),
                            InspectionResponse::Capacity(capacity) => {
                                let reason = capacity.to_string();
                                (
                                    work.plan_inspection_capacity(
                                        root,
                                        &prepared.resources,
                                        &reply.request,
                                        &revision,
                                        capacity,
                                    )?,
                                    InspectionRecordOutcome::Rejected(reason),
                                )
                            }
                            InspectionResponse::Complete(_) => unreachable!(),
                        };
                        prepared.guard_pages(&mut plan);
                        session.apply(plan).into_diagnostic()?;
                        Ok(outcome)
                    }
                }
            })();
            InspectionRecord {
                result,
                duration_ms: started.elapsed().as_millis() as u64,
            }
        })
        .collect();
    Ok(RecordedInspections {
        records,
        revalidation_duration_ms: loaded.duration_ms,
    })
}

#[cfg(unix)]
pub(super) struct SectionFinalization {
    pub request: Value,
    completed: Value,
    result: lk_queue::SectionResult,
    scope: Prepared,
}

#[cfg(unix)]
pub(super) struct ResultFinalization {
    pub request: Value,
    completed: Value,
    kind: state::CompletionKind,
}

#[cfg(unix)]
enum FrontierFinalization {
    Ready {
        result: Box<ResultFinalization>,
        admission: native::FinalizationAdmission,
    },
    Unavailable(native::FinalizationAdmission),
}

#[cfg(unix)]
impl FrontierFinalization {
    fn build(
        work: &Work,
        task: &lk_queue::QueueTask,
        request: Value,
        completed: Value,
        max_input_bytes: usize,
    ) -> miette::Result<Self> {
        let raw = serde_json::to_vec(&request).into_diagnostic()?;
        let request_bytes = raw.len();
        let admission = native::FinalizationAdmission::new(
            work,
            task,
            native::FinalizationMode::Frontier,
            native::FinalizationAdmissionState::Available {
                request_bytes: Some(request_bytes),
            },
        )
        .with_completion(&completed)?
        .with_request_revision(lk_core::rss_original::digest(&raw));
        if request_bytes > max_input_bytes {
            return Ok(Self::Unavailable(admission.with_outcome(
                native::FinalizationAdmissionState::NativeInputCapacity {
                    request_bytes,
                    max_input_bytes,
                    canonical_input_bytes: serde_json::to_vec(&completed).into_diagnostic()?.len(),
                    request_components: native::FinalizationInputBytes::build(
                        &request,
                        request_bytes,
                    )?,
                },
            )));
        }
        Ok(Self::Ready {
            result: Box::new(ResultFinalization {
                request,
                completed,
                kind: state::CompletionKind::Frontier,
            }),
            admission,
        })
    }
}

#[cfg(unix)]
fn frontier_result(
    root: &Path,
    config: &lk_core::config::Config,
    task: &lk_queue::QueueTask,
    work: &Work,
    prepared: &Prepared,
) -> miette::Result<FrontierFinalization> {
    let snapshot = match work.capture_frontier(root, &prepared.resources)? {
        state::FrontierCapture::Ready(snapshot) => snapshot,
        state::FrontierCapture::Unavailable(reason) => {
            return Ok(FrontierFinalization::Unavailable(
                native::FinalizationAdmission::new(
                    work,
                    task,
                    native::FinalizationMode::Frontier,
                    native::FinalizationAdmissionState::FrontierUnavailable { reason },
                ),
            ));
        }
    };
    let completed = snapshot.result().clone();
    let request = match native::frontier_request(
        config,
        task,
        work,
        &generation::Owners::build(config, prepared)?,
        prepared.roster.as_ref(),
        &completed,
    ) {
        Ok(request) => request,
        Err(error) => {
            let Some(capacity) = error.downcast_ref::<generation::ResultCapacity>() else {
                return Err(error);
            };
            return Ok(FrontierFinalization::Unavailable(
                native::FinalizationAdmission::new(
                    work,
                    task,
                    native::FinalizationMode::Frontier,
                    native::FinalizationAdmissionState::NativeResultCapacity {
                        minimum_bytes: capacity.minimum_bytes,
                        max_result_bytes: capacity.max_result_bytes,
                    },
                )
                .with_completion(&completed)?,
            ));
        }
    };
    FrontierFinalization::build(work, task, request, completed, work.max_input_bytes)
}

#[cfg(unix)]
enum FinalizationHold {
    ProcessingAction(String),
    DiscoveryRecovery(String),
    GenerationRejection(String),
}

#[cfg(unix)]
impl FinalizationHold {
    fn reason(self) -> String {
        match self {
            Self::ProcessingAction(reason)
            | Self::DiscoveryRecovery(reason)
            | Self::GenerationRejection(reason) => reason,
        }
    }
}

#[cfg(unix)]
fn native_finalization_hold(
    root: &Path,
    task: &lk_queue::QueueTask,
    work: &Work,
    request: &Value,
    completed: &Value,
) -> miette::Result<Option<FinalizationHold>> {
    let action = work.load_finalization_action_for(root, request, &completed["result_revision"])?;
    let recovery = discovery::recovery::blocked(root, task, request)?;
    if let Some(reason) = generation::rejection::held(root, task, &work.plan.subject_revision)? {
        return Ok(Some(FinalizationHold::GenerationRejection(reason)));
    }
    Ok(recovery
        .map(FinalizationHold::DiscoveryRecovery)
        .or_else(|| action.map(FinalizationHold::ProcessingAction)))
}

#[cfg(unix)]
fn prepare_frontier_finalization(
    root: &Path,
    config: &lk_core::config::Config,
    work: &Work,
    prepared: &Prepared,
) -> miette::Result<Option<FinalizationPreparation>> {
    let Subject::Queue { task_id, .. } = &prepared.subject else {
        return Ok(None);
    };
    let task = super::queue::current_task(root, task_id)?;
    let FrontierFinalization::Ready { result, .. } =
        frontier_result(root, config, &task, work, prepared)?
    else {
        return Ok(None);
    };
    if let Some(hold) =
        native_finalization_hold(root, &task, work, &result.request, &result.completed)?
    {
        return Ok(Some(FinalizationPreparation::NeedsAction(hold.reason())));
    }
    Ok(Some(FinalizationPreparation::Result(result)))
}

#[cfg(unix)]
pub(super) enum FinalizationPreparation {
    Ready(Box<SectionFinalization>),
    Result(Box<ResultFinalization>),
    NeedsAction(String),
    Capacity {
        request_bytes: usize,
        max_input_bytes: usize,
    },
}

#[cfg(unix)]
pub(super) enum NativeFinalization {
    Finished,
    Ready(FinalizationPreparation),
    Replan,
}

#[cfg(unix)]
pub(super) struct FinalizationCheckpoint {
    pub state: NativeFinalization,
    pub preparation_duration_ms: u64,
    pub verification_duration_ms: u64,
}

#[cfg(unix)]
pub(super) fn prepare_native_finalization(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    frontier: bool,
    read: &lk_vault::ReadControl,
) -> miette::Result<FinalizationCheckpoint> {
    let started_at = std::time::Instant::now();
    let root = session.root();
    let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
    let capture_duration_ms = started_at.elapsed().as_millis() as u64;
    let mut checkpoint = prepare_native_finalization_from_capture(
        root,
        config,
        config_revision,
        &work,
        &prepared,
        frontier,
    )?;
    checkpoint.preparation_duration_ms += capture_duration_ms;
    read.check().into_diagnostic()?;
    Ok(checkpoint)
}

#[cfg(unix)]
fn prepare_native_finalization_from_capture(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    work: &Work,
    prepared: &Prepared,
    frontier: bool,
) -> miette::Result<FinalizationCheckpoint> {
    let started_at = std::time::Instant::now();
    let finished = match is_finished_prepared(root, config, config_revision, work, prepared) {
        Ok(finished) => Ok(finished),
        Err(error) => match super::audit::assessment_blocker(&error) {
            Some(reason) => Err(reason),
            None => return Err(error),
        },
    };
    let verification_duration_ms = started_at.elapsed().as_millis() as u64;
    let started_at = std::time::Instant::now();
    let state = match finished {
        Err(reason) => NativeFinalization::Ready(FinalizationPreparation::NeedsAction(reason)),
        Ok(true) => NativeFinalization::Finished,
        Ok(false) => match with_current_roster(
            root,
            config,
            config_revision,
            prepared,
            prepared.read_control(),
        )? {
            RosterAdmission::Changed(_) => NativeFinalization::Replan,
            RosterAdmission::Bound(prepared) if frontier => {
                match prepare_frontier_finalization(root, config, work, &prepared)? {
                    Some(preparation) => NativeFinalization::Ready(preparation),
                    None => NativeFinalization::Replan,
                }
            }
            RosterAdmission::Bound(prepared) => NativeFinalization::Ready(
                prepare_finalization_from_capture(root, config, config_revision, work, &prepared)?,
            ),
        },
    };
    Ok(FinalizationCheckpoint {
        state,
        preparation_duration_ms: started_at.elapsed().as_millis() as u64,
        verification_duration_ms,
    })
}

#[cfg(unix)]
pub(super) fn finalization_capacity_reason(request_bytes: usize, max_input_bytes: usize) -> String {
    format!(
        "complete finalizer request requires {request_bytes} bytes, above max_input_bytes {max_input_bytes}; existing work is retained"
    )
}

#[cfg(unix)]
fn event_headings(body: &str) -> Vec<String> {
    lk_core::markdown::root_headings(body)
        .into_iter()
        .filter(|heading| heading.level as usize == 3)
        .map(|heading| body[heading.range].trim_end().to_owned())
        .collect()
}

#[cfg(unix)]
fn refinement_events(
    root: &Path,
    prepared: &Prepared,
    task: &lk_queue::QueueTask,
) -> miette::Result<Value> {
    let owner = lk_vault::page_path(root, Path::new(&task.target.vault_path))
        .into_diagnostic()?
        .canonicalize()
        .into_diagnostic()?;
    let owner = owner.strip_prefix(root).into_diagnostic()?;
    let raw = prepared
        .pages
        .get(owner)
        .ok_or_else(|| miette::miette!("refinement target is absent from the captured scope"))?;
    let anchor = task
        .target
        .anchor
        .strip_prefix("## ")
        .ok_or_else(|| miette::miette!("invalid refinement section anchor"))?;
    let body = lk_vault::section_body(raw, anchor)
        .ok_or_else(|| miette::miette!("missing refinement event section"))?;
    let headings = lk_core::markdown::root_headings(body)
        .into_iter()
        .filter(|heading| heading.level as usize == 3)
        .collect::<Vec<_>>();
    let events = headings
        .iter()
        .enumerate()
        .map(|(index, heading)| {
            let end = headings
                .get(index + 1)
                .map_or(body.len(), |next| next.range.start);
            let mut links = Vec::new();
            lk_core::link::rewrite_links_outside_code(&body[heading.range.end..end], |link| {
                links.push(format!("[{}]({})", link.raw_text, link.raw_destination));
                None
            });
            json!({"heading":body[heading.range.clone()].trim_end(), "inline_links":links})
        })
        .collect::<Vec<_>>();
    Ok(json!(events))
}

#[cfg(all(unix, test))]
pub(super) fn prepare_finalization(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<FinalizationPreparation> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, prepared) = load(&root, config, config_revision, id)?;
    prepare_finalization_from_capture(&root, config, config_revision, &work, &prepared)
}

#[cfg(unix)]
fn prepare_finalization_from_capture(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    work: &Work,
    prepared: &Prepared,
) -> miette::Result<FinalizationPreparation> {
    let completed = match work.result(root, &prepared.resources) {
        Ok(completed) => completed,
        Err(error) => {
            let Some(capacity) = error.downcast_ref::<state::InputCapacity>() else {
                return Err(error);
            };
            return Ok(FinalizationPreparation::Capacity {
                request_bytes: capacity.actual_bytes,
                max_input_bytes: capacity.max_bytes,
            });
        }
    };
    let Subject::Queue {
        task_id,
        cache_hash,
        target_revision,
        ..
    } = &prepared.subject
    else {
        let request = audit::request(work, prepared, &completed)?;
        let request_bytes = serde_json::to_vec(&request).into_diagnostic()?.len();
        if request_bytes > work.max_input_bytes {
            return Ok(FinalizationPreparation::Capacity {
                request_bytes,
                max_input_bytes: work.max_input_bytes,
            });
        }
        if let Some(reason) =
            audit::assessment_binding(prepared, &prepared.revision(config_revision)?, &request)?
                .held(root)?
                .or(work.load_finalization_action_for(
                    root,
                    &request,
                    &completed["result_revision"],
                )?)
        {
            return Ok(FinalizationPreparation::NeedsAction(reason));
        }
        return Ok(FinalizationPreparation::Result(Box::new(
            ResultFinalization {
                request,
                completed,
                kind: state::CompletionKind::Root,
            },
        )));
    };
    let task = super::queue::current_task_controlled(root, task_id, prepared.read_control())?;
    if !super::execution::finalization::supports(task.kind) {
        let request = match native::finalization_request(
            config,
            &task,
            work,
            &generation::Owners::build(config, prepared)?,
            prepared.roster.as_ref(),
            &work.plan.subject_revision,
            &completed,
        ) {
            Ok(request) => request,
            Err(error) if error.downcast_ref::<generation::ResultCapacity>().is_some() => {
                return Ok(FinalizationPreparation::NeedsAction(error.to_string()));
            }
            Err(error) => return Err(error),
        };
        let request_bytes = serde_json::to_vec(&request).into_diagnostic()?.len();
        if request_bytes > work.max_input_bytes {
            return Ok(FinalizationPreparation::Capacity {
                request_bytes,
                max_input_bytes: work.max_input_bytes,
            });
        }
        if let Some(hold) = native_finalization_hold(root, &task, work, &request, &completed)? {
            return Ok(FinalizationPreparation::NeedsAction(hold.reason()));
        }
        if completed["output"]["unresolved"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        {
            return Ok(FinalizationPreparation::NeedsAction(
                "unresolved work concerns prevent native finalization".into(),
            ));
        }
        return Ok(FinalizationPreparation::Result(Box::new(
            ResultFinalization {
                request,
                completed,
                kind: state::CompletionKind::Root,
            },
        )));
    }
    let scope = resources::section_task_controlled(root, config, task_id, prepared.read_control())?;
    let result = lk_queue::SectionResult {
        context_revision: Some(scope.revision(config_revision)?),
        task_id: task_id.clone(),
        cache_hash: cache_hash.clone(),
        target_revision: target_revision.clone(),
        body: String::new(),
    };
    let envelope_bytes = serde_json::to_vec(&result).into_diagnostic()?.len();
    let response_bytes = super::execution::finalization::empty_response_bytes();
    let max_response_bytes = work
        .plan
        .max_result_bytes
        .checked_sub(envelope_bytes)
        .and_then(|limit| limit.checked_add(response_bytes))
        .ok_or_else(|| miette::miette!("section result identity exceeds max_result_bytes"))?;
    let events = (task.kind == lk_queue::TaskKind::RefineEvents)
        .then(|| refinement_events(root, prepared, &task))
        .transpose()?;
    let review_resources = scope
        .resources
        .iter()
        .filter(|resource| {
            !prepared
                .resources
                .iter()
                .any(|original| original == *resource)
        })
        .collect::<Vec<_>>();
    let review_context = json!({
        "context_revision":result.context_revision,
        "dependencies":scope.context["purpose"]["review_dependencies"],
        "resources":review_resources,
    });
    let mut request = super::execution::finalization::request(
        &completed,
        events,
        max_response_bytes,
        review_context,
    )?;
    request["source_read"] = json!(source::descriptor(work)?);
    let request_bytes = serde_json::to_vec(&request).into_diagnostic()?.len();
    let max_input_bytes = config
        .execution
        .effective_work()
        .map_err(|error| miette::miette!("{error}"))?
        .max_input_bytes;
    if request_bytes > max_input_bytes {
        return Ok(FinalizationPreparation::Capacity {
            request_bytes,
            max_input_bytes,
        });
    }
    if let Some(reason) = generation::rejection::held(root, &task, &work.plan.subject_revision)?
        .or(work.load_finalization_action(root, &request)?)
    {
        return Ok(FinalizationPreparation::NeedsAction(reason));
    }
    if completed["output"]["unresolved"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        return Ok(FinalizationPreparation::NeedsAction(
            "unresolved work concerns prevent section finalization".into(),
        ));
    }
    Ok(FinalizationPreparation::Ready(Box::new(
        SectionFinalization {
            request,
            completed,
            result,
            scope,
        },
    )))
}

#[cfg(unix)]
pub(super) enum ResultFinish {
    /// Staged, with the reason a review proposal was refused when its synthesis published.
    Staged(Option<String>),
    NeedsAction(String),
    DiscoveryRecorded,
    ReviewRecorded(String),
    ReviewDeferred(String),
    Rejected(String),
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_result(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    result: ResultFinalization,
    reply: Value,
    review: generation::ReviewAllowance,
    read: &lk_vault::ReadControl,
) -> miette::Result<ResultFinish> {
    let root = session.root();
    let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
    let prepared = require_current_roster(root, config, config_revision, &prepared, read)?;
    let completed =
        validate_result_finalization(root, config, &work, &prepared, &result.request, result.kind)?;
    if completed != result.completed {
        return Err(publication_rejection(
            RejectionKind::SemanticContextChanged,
            "completed Work changed before native finalization",
        ));
    }
    let (outcome, refused_review) = match &prepared.subject {
        Subject::Audit { .. } => (audit::assemble(&prepared, reply.clone())?, None),
        Subject::Queue { task_id, .. } => generation::assemble_prepared(
            root,
            config,
            &super::queue::current_task(root, task_id)?,
            &prepared,
            &work,
            reply.clone(),
            review,
        )?,
    };
    match outcome {
        generation::Outcome::Rejected(reason) => {
            let Subject::Queue { task_id, .. } = &prepared.subject else {
                return Err(miette::miette!(
                    "native owner rejection requires a queue subject"
                ));
            };
            let task = super::queue::current_task(root, task_id)?;
            let capture = work.capture_native_completion(root, &prepared.resources, result.kind)?;
            if capture.result() != &completed {
                return Err(miette::miette!(
                    "completed Work changed before generation rejection"
                ));
            }
            let mut plan = lk_vault::MutationPlan::default();
            capture.plan_read_guards(&mut plan);
            prepared.guard_pages(&mut plan);
            let reason = generation::rejection::plan_record(
                root,
                generation::rejection::Binding {
                    task: &task,
                    work: &work,
                    input: generation::rejection::Input::Work {
                        work_id: work.plan.id.clone(),
                    },
                    request: &result.request,
                },
                generation::rejection::Cause::proposal(reply, reason)?,
                &mut plan,
            )?;
            session.apply(plan).into_diagnostic()?;
            Ok(ResultFinish::Rejected(reason))
        }
        generation::Outcome::NeedsDiscovery(recovery, reason) => {
            let Subject::Queue { task_id, .. } = &prepared.subject else {
                return Err(miette::miette!(
                    "discovery recovery requires a queue subject"
                ));
            };
            let task = super::queue::current_task(root, task_id)?;
            let capture = work.capture_native_completion(root, &prepared.resources, result.kind)?;
            if capture.result() != &completed {
                return Err(miette::miette!(
                    "completed Work changed before discovery recovery"
                ));
            }
            let mut plan = lk_vault::MutationPlan::default();
            capture.plan_read_guards(&mut plan);
            discovery::recovery::record(
                session,
                discovery::recovery::Binding {
                    task: &task,
                    prepared: &prepared,
                    request: &result.request,
                    context_revision: &work.plan.subject_revision,
                    work_id: &work.plan.id,
                },
                recovery,
                reason,
                plan,
            )?;
            Ok(ResultFinish::DiscoveryRecorded)
        }
        generation::Outcome::NeedsReview(..) => {
            unreachable!("assemble_prepared admits or refuses every review")
        }
        generation::Outcome::ReviewDeferred(reason) => Ok(ResultFinish::ReviewDeferred(reason)),
        generation::Outcome::ReviewAdmitted(review) => {
            let Subject::Queue { task_id, .. } = &prepared.subject else {
                return Err(miette::miette!(
                    "a review proposal requires a queue subject"
                ));
            };
            let task = super::queue::current_task(root, task_id)?;
            let capture = work.capture_native_completion(root, &prepared.resources, result.kind)?;
            if capture.result() != &completed {
                return Err(miette::miette!(
                    "completed Work changed before its review proposal"
                ));
            }
            let mut plan = lk_vault::MutationPlan::default();
            capture.plan_read_guards(&mut plan);
            let path = review::record(session, &task, &prepared, review, plan)?;
            Ok(ResultFinish::ReviewRecorded(path))
        }
        generation::Outcome::NeedsAction(reason) => {
            let mut plan = work.plan_finalization_action_for(
                root,
                &prepared.resources,
                &completed,
                &result.request,
                reason.clone(),
                result.kind,
            )?;
            prepared.guard_pages(&mut plan);
            session.apply(plan).into_diagnostic()?;
            Ok(ResultFinish::NeedsAction(reason))
        }
        generation::Outcome::Complete(value) => {
            let (_, plan) = plan_finish_completion(
                session,
                &work,
                &prepared,
                value,
                Some(&completed),
                result.kind,
            )?;
            session.apply(plan).into_diagnostic()?;
            Ok(ResultFinish::Staged(refused_review))
        }
    }
}

#[cfg(unix)]
pub(super) fn record_finalization_action(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    section: SectionFinalization,
    reason: String,
    read: &lk_vault::ReadControl,
) -> miette::Result<()> {
    let root = session.root();
    let (work, prepared) =
        revalidate_section_finalization(root, config, config_revision, id, &section, read)?;
    let mut plan = work.plan_finalization_action(
        root,
        &prepared.resources,
        &section.completed,
        &section.request,
        reason,
    )?;
    prepared.guard_pages(&mut plan);
    section.scope.guard_pages(&mut plan);
    session.apply(plan).into_diagnostic()?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn finish_section(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    prepared_section: SectionFinalization,
    body: String,
    read: &lk_vault::ReadControl,
) -> miette::Result<()> {
    let root = session.root();
    validate_section_response(&prepared_section.request, &body)?;
    let (work, prepared) = revalidate_section_finalization(
        root,
        config,
        config_revision,
        id,
        &prepared_section,
        read,
    )?;
    let result = lk_queue::SectionResult {
        body,
        ..prepared_section.result
    };
    let (_, mut mutation) = plan_finish_expected(
        session,
        &work,
        &prepared,
        serde_json::to_value(result).into_diagnostic()?,
        Some(&prepared_section.completed),
    )?;
    prepared_section.scope.guard_pages(&mut mutation);
    session.apply(mutation).into_diagnostic()?;
    Ok(())
}

#[cfg(unix)]
fn revalidate_section_finalization(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    section: &SectionFinalization,
    read: &lk_vault::ReadControl,
) -> miette::Result<(Work, Prepared)> {
    let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
    current_section_finalization(root, config, config_revision, work, prepared, section)?
        .ok_or_else(|| {
            publication_rejection(
                RejectionKind::SemanticContextChanged,
                "section finalization scope changed during generation; retained work requires a fresh request",
            )
        })
}

/// The loaded Work while its current section finalizer is still `section`, or `None` when the
/// scope or completed observations changed after the request was prepared.
#[cfg(unix)]
fn current_section_finalization(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    work: Work,
    prepared: Prepared,
    section: &SectionFinalization,
) -> miette::Result<Option<(Work, Prepared)>> {
    let FinalizationPreparation::Ready(current) =
        prepare_finalization_from_capture(root, config, config_revision, &work, &prepared)?
    else {
        return Ok(None);
    };
    Ok(
        (current.request == section.request && current.completed == section.completed)
            .then_some((work, prepared)),
    )
}

/// Holds a bounded finalizer whose tool-enabled host failed, for its exact current input, and
/// returns the target's hold; `None` when the Work's fresh inputs no longer produce the failed
/// request.
#[cfg(unix)]
pub(super) fn hold_result(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    result: &ResultFinalization,
    failure: state::InvocationFailure,
    read: &lk_vault::ReadControl,
) -> miette::Result<Option<String>> {
    let root = session.root();
    let (work, prepared) = match load_controlled(root, config, config_revision, id, read) {
        Ok(loaded) => loaded,
        Err(error) if is_work_input_changed(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let prepared = match with_current_roster(root, config, config_revision, &prepared, read)? {
        RosterAdmission::Bound(prepared) => *prepared,
        RosterAdmission::Changed(_) => return Ok(None),
    };
    let Some(completed) =
        current_result_finalization(root, config, &work, &prepared, &result.request, result.kind)?
    else {
        return Ok(None);
    };
    let mut plan = lk_vault::MutationPlan::default();
    completed.plan_read_guards(&mut plan);
    prepared.guard_pages(&mut plan);
    match &prepared.subject {
        Subject::Queue { task_id, .. } => {
            let task = match super::queue::current_task(root, task_id) {
                Ok(task) => task,
                Err(error) if is_work_input_changed(&error) => return Ok(None),
                Err(error) => return Err(error),
            };
            let reason = generation::rejection::plan_record(
                root,
                generation::rejection::Binding {
                    task: &task,
                    work: &work,
                    input: generation::rejection::Input::Work {
                        work_id: work.plan.id.clone(),
                    },
                    request: &result.request,
                },
                generation::rejection::Cause::invocation(failure),
                &mut plan,
            )?;
            session.apply(plan).into_diagnostic()?;
            Ok(Some(format!("{}: {reason}", task.task_id)))
        }
        Subject::Audit { path, .. } => {
            let binding = audit::assessment_binding(
                &prepared,
                &prepared.revision(config_revision)?,
                &result.request,
            )?;
            plan.extend(
                binding.plan_record(root, preparation::Disposition::invocation_failed(failure))?,
            );
            session.apply(plan).into_diagnostic()?;
            let reason = binding
                .held(root)?
                .ok_or_else(|| miette::miette!("audit assessment hold changed after recording"))?;
            Ok(Some(format!("{path}: {reason}")))
        }
    }
}

/// Holds a bounded section finalizer whose tool-enabled host failed, for its exact current
/// input, and returns the task's hold; `None` when the Work's fresh inputs no longer produce the
/// failed request.
#[cfg(unix)]
pub(super) fn hold_section(
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    section: &SectionFinalization,
    failure: state::InvocationFailure,
    read: &lk_vault::ReadControl,
) -> miette::Result<Option<String>> {
    let root = session.root();
    let (work, prepared) = match load_controlled(root, config, config_revision, id, read) {
        Ok(loaded) => loaded,
        Err(error) if is_work_input_changed(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some((work, prepared)) =
        current_section_finalization(root, config, config_revision, work, prepared, section)?
    else {
        return Ok(None);
    };
    let Subject::Queue { task_id, .. } = &prepared.subject else {
        return Err(miette::miette!(
            "section finalization requires a queue subject"
        ));
    };
    let task = match super::queue::current_task(root, task_id) {
        Ok(task) => task,
        Err(error) if is_work_input_changed(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut plan = lk_vault::MutationPlan::default();
    prepared.guard_pages(&mut plan);
    section.scope.guard_pages(&mut plan);
    let reason = generation::rejection::plan_record(
        root,
        generation::rejection::Binding {
            task: &task,
            work: &work,
            input: generation::rejection::Input::Work {
                work_id: work.plan.id.clone(),
            },
            request: &section.request,
        },
        generation::rejection::Cause::invocation(failure),
        &mut plan,
    )?;
    session.apply(plan).into_diagnostic()?;
    Ok(Some(format!("{}: {reason}", task.task_id)))
}

#[cfg(unix)]
fn validate_section_response(request: &Value, body: &str) -> miette::Result<()> {
    lk_core::markdown::validate_section_body(body).into_diagnostic()?;
    if let Some(events) = request["events"].as_array() {
        let expected = events
            .iter()
            .map(|event| {
                event["heading"]
                    .as_str()
                    .expect("native event heading")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        if expected != event_headings(body) {
            return Err(miette::miette!(
                "refinement must preserve the event heading sequence"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn verify_finished(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<()> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, prepared) = load(&root, config, config_revision, id)?;
    verify_finished_prepared(&root, &work, &prepared)
}

fn verify_finished_prepared(root: &Path, work: &Work, prepared: &Prepared) -> miette::Result<()> {
    let path = Path::new(".lorekeeper/work")
        .join(&work.plan.id)
        .join("final.json");
    let (_, result) = state::load_result(root, &path)?
        .ok_or_else(|| miette::miette!("work final result is missing"))?;
    work.guard_completion(
        &mut lk_vault::MutationPlan::default(),
        root,
        &prepared.resources,
        &result,
    )
}

#[cfg(test)]
fn is_finished(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
) -> miette::Result<bool> {
    let root = root.canonicalize().into_diagnostic()?;
    let (work, prepared) = load(&root, config, config_revision, id)?;
    is_finished_prepared(&root, config, config_revision, &work, &prepared)
}

#[cfg(any(unix, test))]
fn is_finished_prepared(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    work: &Work,
    prepared: &Prepared,
) -> miette::Result<bool> {
    let directory = Path::new(".lorekeeper/work").join(&work.plan.id);
    match state::load_result(root, &directory.join("final.json"))? {
        None => {
            for seal in ["seal.json", "frontier-seal.json"] {
                if state::read_optional(root, &directory.join(seal))?.is_some() {
                    return Err(miette::miette!(
                        "completion seal exists without a final result"
                    ));
                }
            }
            Ok(false)
        }
        Some((_, result)) => match work.guard_completion(
            &mut lk_vault::MutationPlan::default(),
            root,
            &prepared.resources,
            &result,
        ) {
            Ok(()) => {
                let mut guards = lk_vault::MutationPlan::default();
                match guard_section_context(
                    root,
                    config,
                    config_revision,
                    prepared,
                    &result,
                    &mut guards,
                ) {
                    Ok(()) => {
                        if matches!(prepared.subject, Subject::Audit { .. }) {
                            if let Some((_, staged)) =
                                state::load_result(root, &directory.join("assessment.json"))?
                                && staged != result
                            {
                                return Err(miette::miette!(
                                    "staged result differs from the verified sealed result; retained for inspection"
                                ));
                            }
                            prepared.validate_audit_result(&result)?;
                        }
                        Ok(true)
                    }
                    Err(error)
                        if error
                            .downcast_ref::<super::queue::application::PublicationRejection>()
                            .is_some_and(|error| {
                                error.kind == RejectionKind::SemanticContextChanged
                            }) =>
                    {
                        if state::read_optional(root, &directory.join("publication.json"))?
                            .is_some()
                        {
                            return Err(miette::miette!(
                                "published section completion cannot be replaced"
                            ));
                        }
                        if work.plan.max_part_characters.is_none() {
                            return Err(miette::miette!(
                                "legacy scoped section completion requires explicit regeneration"
                            ));
                        }
                        Ok(false)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error)
                if matches!(
                    error.downcast_ref::<state::CompletionRefusal>(),
                    Some(state::CompletionRefusal::ChangedObservations)
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        },
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
    session: &lk_vault::MutationSession,
    config: &lk_core::config::Config,
    config_revision: &str,
    id: &str,
    read: &lk_vault::ReadControl,
) -> miette::Result<()> {
    let root = session.root();
    let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
    let final_path = PathBuf::from(".lorekeeper/work")
        .join(id)
        .join("final.json");
    let (final_raw, result) = state::load_result(root, &final_path)?
        .ok_or_else(|| miette::miette!("work final result is missing"))?;
    let mut mutation = lk_vault::MutationPlan::default();
    work.guard_completion(&mut mutation, root, &prepared.resources, &result)?;
    guard_section_context(
        root,
        config,
        config_revision,
        &prepared,
        &result,
        &mut mutation,
    )?;
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
    let existing = state::load_result(root, &path)?;
    if existing
        .as_ref()
        .is_some_and(|(_, staged)| *staged != result)
    {
        return Err(miette::miette!(
            "staged result differs from the verified sealed result; retained for inspection"
        ));
    }
    if matches!(prepared.subject, Subject::Audit { .. }) {
        prepared.validate_audit_result(&result)?;
    }
    if existing.is_some() {
        return Ok(());
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
    Ok(progress_value(work, prepared, status, limit, offset))
}

fn progress_value(
    work: &Work,
    prepared: &Prepared,
    status: state::WorkProgress,
    limit: usize,
    offset: usize,
) -> Value {
    let total_ready = status.ready.len();
    let ready = status
        .ready
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let next_offset = (offset.saturating_add(ready.len()) < total_ready)
        .then_some(offset.saturating_add(ready.len()));
    let ready_workload = ready
        .iter()
        .map(|id| (id, &status.ready_workload[id]))
        .collect::<std::collections::BTreeMap<_, _>>();
    json!({"work_id":work.plan.id,"subject":prepared.subject,
        "context_revision":work.plan.subject_revision,
        "resources":work.plan.resources.len(),"parts":work.plan.parts.len(),
        "completed":status.completed,"total":status.total,"complete":status.complete,
        "root_node":status.root_node,"ready_total":total_ready,"ready":ready,
        "max_input_bytes":work.max_input_bytes,"ready_workload":ready_workload,
        "max_part_characters":work.plan.max_part_characters,
        "limit":limit,"offset":offset,"next_offset":next_offset})
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn update_inspection_attempt(
    root: &Path,
    pin: &super::ConfigurationPin,
    config: &lk_core::config::Config,
    config_revision: &str,
    work_id: &str,
    node_id: &str,
    update: state::AttemptUpdate,
    dry_run: bool,
) -> miette::Result<()> {
    let prepare = |root: &Path| -> miette::Result<lk_vault::MutationPlan> {
        let (work, prepared) = load(root, config, config_revision, work_id)?;
        let request = work.input(root, node_id, &prepared.resources)?;
        let (mut plan, _) = work.plan_inspection_attempt(
            root,
            &prepared.resources,
            &request,
            &super::execution::observation::contract_revision(&request)?,
            update,
        )?;
        prepared.guard_pages(&mut plan);
        Ok(plan)
    };
    if dry_run {
        prepare(root)?.validate_addresses(root).into_diagnostic()?;
    } else {
        let session = super::publication::acquire_mutation_now(root, pin)?;
        session.apply(prepare(session.root())?).into_diagnostic()?;
    }
    Ok(())
}

pub async fn run(opts: &GlobalOptions, command: WorkCommand) -> miette::Result<()> {
    let config_path = find_config(opts)?;
    let (config, config_revision) = load_config_with_revision(&config_path)?;
    let pin = super::ConfigurationPin::from_loaded(config_path, config_revision.clone());
    let root = config.vault.root_path().canonicalize().into_diagnostic()?;
    let output = match command {
        #[cfg(unix)]
        WorkCommand::RetryGeneration {
            task_id,
            input_revision,
            dry_run,
        } => {
            let prepare = |root: &Path| {
                generation::rejection::plan_retry(
                    root,
                    &config,
                    &config_revision,
                    &task_id,
                    &input_revision,
                )
            };
            if dry_run {
                prepare(&root)?
                    .validate_addresses(&root)
                    .into_diagnostic()?;
            } else {
                let session = super::publication::acquire_mutation_now(&root, &pin)?;
                session.apply(prepare(session.root())?).into_diagnostic()?;
            }
            json!({"task_id":task_id,"input_revision":input_revision,"dry_run":dry_run})
        }
        #[cfg(not(unix))]
        WorkCommand::RetryGeneration { .. } => {
            return Err(miette::miette!("native generation retries require Unix"));
        }
        #[cfg(unix)]
        WorkCommand::RetryPreparation {
            stage,
            task_id,
            path,
            input_revision,
            dry_run,
        } => {
            let prepare = |root: &Path| {
                use preparation::{Key, Stage};
                let key = match (stage, &task_id, &path) {
                    (Stage::ExtractionDiscovery | Stage::WholeSection, Some(task_id), None) => {
                        let task = crate::commands::queue::current_task(root, task_id)?;
                        match stage {
                            Stage::ExtractionDiscovery
                                if task.kind == lk_queue::TaskKind::ExtractConcepts =>
                            {
                                Key::ExtractionDiscovery {
                                    task_id: task.task_id,
                                    cache_hash: task.cache_hash,
                                }
                            }
                            Stage::WholeSection
                                if super::execution::finalization::supports(task.kind) =>
                            {
                                Key::WholeSection {
                                    task_id: task.task_id,
                                    cache_hash: task.cache_hash,
                                }
                            }
                            _ => {
                                return Err(miette::miette!(
                                    "preparation stage does not match the current task kind"
                                ));
                            }
                        }
                    }
                    (Stage::AuditDiscovery, None, Some(path)) => {
                        Key::AuditDiscovery { path: path.clone() }
                    }
                    (Stage::AuditScreening, None, Some(path)) => {
                        Key::AuditScreening { path: path.clone() }
                    }
                    (Stage::AuditAssessment, None, Some(path)) => {
                        Key::AuditAssessment { path: path.clone() }
                    }
                    _ => {
                        return Err(miette::miette!(
                            "queue preparation requires --task-id; audit preparation requires --path"
                        ));
                    }
                };
                preparation::plan_retry(root, &key, &input_revision)
            };
            if dry_run {
                prepare(&root)?
                    .validate_addresses(&root)
                    .into_diagnostic()?;
            } else {
                let session = super::publication::acquire_mutation_now(&root, &pin)?;
                session.apply(prepare(session.root())?).into_diagnostic()?;
            }
            json!({"task_id":task_id,"path":path,"input_revision":input_revision,"dry_run":dry_run})
        }
        #[cfg(not(unix))]
        WorkCommand::RetryPreparation { .. } => {
            return Err(miette::miette!("native preparation retries require Unix"));
        }
        #[cfg(unix)]
        WorkCommand::SelectRetry { task_id, dry_run } => {
            let prepare = |root: &Path| {
                current_screening(root, &config, &config_revision, &task_id)?
                    .into_candidates()?
                    .selection
                    .plan_retry(root)
            };
            if dry_run {
                prepare(&root)?
                    .validate_addresses(&root)
                    .into_diagnostic()?;
            } else {
                let session = super::publication::acquire_mutation_now(&root, &pin)?;
                session.apply(prepare(session.root())?).into_diagnostic()?;
            }
            json!({"task_id":task_id,"dry_run":dry_run})
        }
        #[cfg(not(unix))]
        WorkCommand::SelectRetry { .. } => {
            return Err(miette::miette!("native screening retries require Unix"));
        }
        WorkCommand::Cost { task_id } => {
            match current_screening(&root, &config, &config_revision, &task_id)? {
                ScreeningPreparation::Capacity(capacity) => return Err(capacity.into()),
                ScreeningPreparation::SourceBlocked { work_id } => {
                    return Err(miette::miette!("{}", source_blocked_reason(&work_id)));
                }
                ScreeningPreparation::Source { work, .. } => {
                    json!({"status":"needs-source-observations",
                "source_parts":work.plan.parts.len(),"cost":null,
                "authority":"Candidate packet costs are unknown until complete source observations are available; no model call or write was made."})
                }
                ScreeningPreparation::Candidates(screening) => {
                    let reason = screening.selection.admission_reason(&config)?;
                    json!({"status":if reason.is_some() {"needs-admission"} else {"admitted"},
                    "cost":screening.selection.cost(&screening.inventory.capture)?,"reason":reason,
                    "max_screening_batches":config.execution.effective_work().into_diagnostic()?.max_screening_batches,
                    "authority":"Compact UTF-8 JSON bytes and exact packet counts for the undecided candidates, not tokens, billed cost, latency or semantic relevance. Each query lists the pages it matched and how many the ranked roster carries; unrostered matches were not examined. Deferred and unknown candidates remain incomplete."})
                }
            }
        }
        WorkCommand::Select { task_id, reassess } => {
            match current_screening(&root, &config, &config_revision, &task_id)? {
                ScreeningPreparation::Capacity(capacity) => return Err(capacity.into()),
                ScreeningPreparation::SourceBlocked { work_id } => {
                    return Err(miette::miette!("{}", source_blocked_reason(&work_id)));
                }
                ScreeningPreparation::Source { work, mut prepared } => {
                    let session = super::publication::acquire_mutation_now(&root, &pin)?;
                    persist_prepared(&session, &config, &config_revision, &mut prepared)?;
                    json!({
                        "status":"needs-source-observations",
                        "source_work":progress(&work, &root, &prepared, 20, 0)?,
                        "plan_command":["lore","work","source-plan","--task-id",task_id],
                    })
                }
                ScreeningPreparation::Candidates(screening) => {
                    let admission = if reassess {
                        None
                    } else {
                        screening.selection.admission_reason(&config)?
                    };
                    let progress = screening.selection.progress();
                    let request = match &admission {
                        Some(_) => None,
                        None => screening.selection.next_request(reassess)?,
                    };
                    let hold = request
                        .as_ref()
                        .map(|request| screening.selection.hold_reason(request))
                        .transpose()?
                        .flatten();
                    let status = if admission.is_some() {
                        "needs-admission"
                    } else if request.is_some() {
                        "screening"
                    } else if progress.complete {
                        "ready"
                    } else {
                        "deferred"
                    };
                    let selection = (status == "ready")
                        .then(|| seal_roster(&root, &pin, &screening.selection))
                        .transpose()?;
                    json!({"status":status,"admission":admission,"progress":progress,
                        "request":request,"hold":hold,"selection":selection})
                }
            }
        }
        WorkCommand::SelectRecord { task_id, file } => {
            let output: selection::ScreeningOutput =
                receipt::decode(&std::fs::read_to_string(file).into_diagnostic()?)
                    .into_diagnostic()?;
            if output.task_id != task_id {
                return Err(miette::miette!("screening output answers another task"));
            }
            let session = super::publication::acquire_mutation_now(&root, &pin)?;
            let screening = current_screening(session.root(), &config, &config_revision, &task_id)?
                .into_candidates()?;
            let request = screening.selection.request_for(&output)?;
            let recorded = request.candidates();
            session
                .apply(selection::plan_record(session.root(), &request, output)?)
                .into_diagnostic()?;
            json!({"recorded_decisions":recorded,"next_command":["lore","work","select","--task-id",task_id]})
        }
        WorkCommand::SourcePlan { task_id } => {
            let session = super::publication::acquire_mutation_now(&root, &pin)?;
            let mut prepared = resources::source_task(&root, &config, &task_id)?;
            let work = persist_prepared(&session, &config, &config_revision, &mut prepared)?;
            progress(&work, &root, &prepared, 20, 0)?
        }
        WorkCommand::Discover { task_id, file } => {
            let request = serde_json::from_slice(&std::fs::read(file).into_diagnostic()?)
                .into_diagnostic()?;
            let session = super::publication::acquire_mutation_now(&root, &pin)?;
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
            let session = super::publication::acquire_mutation_now(&root, &pin)?;
            let discovery = discovery_file.as_deref().map(load_discovery).transpose()?;
            let mut prepared = match (task_id, path) {
                (Some(id), None) => {
                    task_for_plan(&root, &config, &config_revision, &id, discovery.as_deref())?
                }
                (None, Some(path)) => resources::audit(&root, &config, &path)?,
                _ => return Err(miette::miette!("select exactly one task or audit path")),
            };
            let work = persist_prepared(&session, &config, &config_revision, &mut prepared)?;
            progress(&work, &root, &prepared, 20, 0)?
        }
        WorkCommand::Read { work_id, node_id } => {
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            work.input(&root, &node_id, &prepared.resources)?
        }
        WorkCommand::Source {
            work_id,
            resource,
            offset,
            limit,
            find,
        } => {
            // One planned resource is read and verified against its plan; rebuilding the
            // Work's whole scope for each page would cost a full capture per call.
            let work = Work::load(&root, &work_id)?;
            let index = source::resource_index(&work, resource)?;
            let source = resources::load_planned_controlled(
                &root,
                &work,
                index,
                &lk_vault::ReadControl::default(),
            )?;
            source::read(&work, index, &source, offset, limit, find.as_deref())?
        }
        #[cfg(unix)]
        WorkCommand::Refine {
            work_id,
            node_id,
            max_part_characters,
            dry_run,
        } => {
            let prepare = |root: &Path| -> miette::Result<(
                lk_vault::MutationPlan,
                state::RefinementPreview,
            )> {
                let (work, prepared) = load(root, &config, &config_revision, &work_id)?;
                let (mut mutation, preview) = work.plan_refinement(
                    root,
                    &prepared.resources,
                    &node_id,
                    max_part_characters,
                    super::execution::observation::contract_revision,
                )?;
                prepared.guard_pages(&mut mutation);
                Ok((mutation, preview))
            };
            let preview = if dry_run {
                let (mutation, preview) = prepare(&root)?;
                mutation.validate_addresses(&root).into_diagnostic()?;
                preview
            } else {
                let session = super::publication::acquire_mutation_now(&root, &pin)?;
                let (mutation, preview) = prepare(session.root())?;
                session.apply(mutation).into_diagnostic()?;
                preview
            };
            let mut value = serde_json::to_value(preview).into_diagnostic()?;
            value["dry_run"] = json!(dry_run);
            value
        }
        #[cfg(not(unix))]
        WorkCommand::Refine { .. } => {
            return Err(miette::miette!(
                "inspection refinement requires Unix native execution"
            ));
        }
        #[cfg(unix)]
        WorkCommand::Retry {
            work_id,
            node_id,
            dry_run,
        } => {
            update_inspection_attempt(
                &root,
                &pin,
                &config,
                &config_revision,
                &work_id,
                &node_id,
                state::AttemptUpdate::Retry,
                dry_run,
            )?;
            json!({"work_id":work_id,"node_id":node_id,"dry_run":dry_run})
        }
        #[cfg(unix)]
        WorkCommand::Hold {
            work_id,
            node_id,
            reason,
            dry_run,
        } => {
            update_inspection_attempt(
                &root,
                &pin,
                &config,
                &config_revision,
                &work_id,
                &node_id,
                state::AttemptUpdate::Hold(reason),
                dry_run,
            )?;
            json!({"work_id":work_id,"node_id":node_id,"dry_run":dry_run})
        }
        #[cfg(not(unix))]
        WorkCommand::Retry { .. } | WorkCommand::Hold { .. } => {
            return Err(miette::miette!(
                "inspection operational holds require Unix native execution"
            ));
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
            let session = super::publication::acquire_mutation_now(&root, &pin)?;
            let (work, prepared) = load(&root, &config, &config_revision, &output.work_id)?;
            let mut mutation = work.plan_observation(&session, output, &prepared.resources)?;
            prepared.guard_pages(&mut mutation);
            session.apply(mutation).into_diagnostic()?;
            let status = work.progress(&root, &prepared.resources)?;
            progress_value(&work, &prepared, status, 20, 0)
        }
        WorkCommand::Reuse {
            work_id,
            node_id,
            from_work,
            from_node,
        } => {
            let session = super::publication::acquire_mutation_now(&root, &pin)?;
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            let prior = Work::load(&root, &from_work)?;
            let mut mutation =
                work.plan_reuse(&session, &prior, &from_node, &node_id, &prepared.resources)?;
            prepared.guard_pages(&mut mutation);
            session.apply(mutation).into_diagnostic()?;
            let status = work.progress(&root, &prepared.resources)?;
            progress_value(&work, &prepared, status, 20, 0)
        }
        WorkCommand::Finish {
            work_id,
            file,
            prepared: request,
        } => {
            let result = lk_core::json::decode_object(&std::fs::read(file).into_diagnostic()?)
                .into_diagnostic()?;
            let session = super::publication::acquire_mutation_now(&root, &pin)?;
            let (work, prepared) = load(&root, &config, &config_revision, &work_id)?;
            #[cfg(unix)]
            {
                let inherited = match (
                    std::env::var_os(FINALIZATION_REQUEST_ENV),
                    std::env::var_os(FINALIZATION_REVISION_ENV),
                ) {
                    (None, None) => None,
                    (Some(path), Some(revision)) => Some((
                        PathBuf::from(path),
                        revision.into_string().map_err(|_| {
                            miette::miette!("invalid native finalizer request revision")
                        })?,
                    )),
                    _ => {
                        return Err(miette::miette!(
                            "incomplete native finalizer request binding"
                        ));
                    }
                };
                let expected = load_native_finalization_input(
                    request.as_deref(),
                    inherited
                        .as_ref()
                        .map(|(path, revision)| (path.as_path(), revision.as_str())),
                )?;
                let prepared = match &expected {
                    Some(_) => require_current_roster(
                        &root,
                        &config,
                        &config_revision,
                        &prepared,
                        prepared.read_control(),
                    )?,
                    None => std::borrow::Cow::Borrowed(&prepared),
                };
                finish_with_preparation(
                    &session,
                    &config,
                    &work,
                    &prepared,
                    result,
                    expected.as_ref(),
                )?
            }
            #[cfg(not(unix))]
            {
                if request.is_some() {
                    return Err(miette::miette!(
                        "native prepared finalization is supported only on Unix"
                    ));
                }
                finish(&session, &work, &prepared, result)?
            }
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
    plan_finish_expected(session, work, prepared, result, None)
}

fn plan_finish_expected(
    session: &lk_vault::MutationSession,
    work: &Work,
    prepared: &Prepared,
    result: Value,
    expected: Option<&Value>,
) -> miette::Result<(PathBuf, lk_vault::MutationPlan)> {
    plan_finish_completion(
        session,
        work,
        prepared,
        result,
        expected,
        state::CompletionKind::Root,
    )
}

fn plan_finish_completion(
    session: &lk_vault::MutationSession,
    work: &Work,
    prepared: &Prepared,
    result: Value,
    expected: Option<&Value>,
    kind: state::CompletionKind,
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
    let completed = work.capture_native_completion(session.root(), &prepared.resources, kind)?;
    if expected.is_some_and(|expected| expected != completed.result()) {
        return Err(miette::miette!(
            "completed observations changed during section finalization; result retained without publication"
        ));
    }
    let unresolved = completed.result()["output"]["unresolved"]
        .as_array()
        .is_some_and(|items| !items.is_empty());
    let mut sealed = None;
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
                    let sealing = prepared.roster.as_ref().map(|roster| &roster.seal);
                    sealed = typed
                        .selection
                        .as_ref()
                        .map(|selection| {
                            selection::load_sealed(
                                session.root(),
                                task_id,
                                cache_hash,
                                selection,
                                sealing,
                            )
                        })
                        .transpose()?;
                    let admits_evidence = match &sealed {
                        Some(sealed) => {
                            Some(&sealed.selected) == prepared.selected_owners.as_ref()
                                && sealed.queries == typed.discovery
                        }
                        None => true,
                    };
                    if sealed.is_some() != resources::is_screened(&prepared.context)
                        || !admits_evidence
                    {
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
    let staged = state::load_result(session.root(), &path)?;
    let mut mutation = lk_vault::MutationPlan::default();
    prepared.guard_pages(&mut mutation);
    if let Some(sealed) = &sealed {
        sealed.guard(&mut mutation);
        if let Some(roster) = &prepared.roster {
            roster.seal.plan(session.root(), &mut mutation)?;
        }
    }
    let final_before = completed.plan_seal(work, &mut mutation, session.root(), &result)?;
    if let Some((_, value)) = &staged
        && value != &result
        && (!matches!(prepared.subject, Subject::Audit { .. })
            || final_before
                .as_deref()
                .map(|raw| lk_core::json::decode_object(raw.as_bytes()))
                .transpose()
                .map_err(|error| miette::miette!("{error}"))?
                .as_ref()
                != Some(value))
    {
        return Err(miette::miette!(
            "staged result differs from the captured prior or requested completion"
        ));
    }
    let raw = serde_json::to_string(&result).into_diagnostic()?;
    let final_path = PathBuf::from(".lorekeeper/work")
        .join(&work.plan.id)
        .join("final.json");
    for (path, before) in [
        (final_path, final_before),
        (path.clone(), staged.map(|(raw, _)| raw)),
    ] {
        let after = match &before {
            Some(before)
                if lk_core::json::decode_object(before.as_bytes())
                    .map_err(|error| miette::miette!("{error}"))?
                    == result =>
            {
                before.clone()
            }
            _ => raw.clone(),
        };
        if before.as_ref() == Some(&after) {
            mutation.require_unchanged(path, after);
        } else {
            mutation
                .publish_expected(session.root(), path, before, after)
                .into_diagnostic()?;
        }
    }
    Ok((path, mutation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::concept::file_revision;
    use lk_vault::{MutationPlan, MutationSession};

    fn record_inspection(
        session: &lk_vault::MutationSession,
        config: &lk_core::config::Config,
        config_revision: &str,
        id: &str,
        node: &str,
        output: WorkOutput,
        read: &lk_vault::ReadControl,
    ) -> miette::Result<state::WorkProgress> {
        if output.work_id != id || output.node_id != node {
            return Err(miette::miette!(
                "observation does not answer the selected work node"
            ));
        }
        let root = session.root();
        let (work, prepared) = load_controlled(root, config, config_revision, id, read)?;
        let mut mutation = work.plan_observation(session, output, &prepared.resources)?;
        prepared.guard_pages(&mut mutation);
        session.apply(mutation).into_diagnostic()?;
        work.progress(root, &prepared.resources)
    }

    fn write(root: &Path, path: &str, raw: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, raw).unwrap();
    }

    #[test]
    fn large_source_is_observed_then_screened_and_sealed_against_its_observations() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let config_path = root.join("config.yaml");
        std::fs::write(&config_path, format!("vault:\n  root: {}\n  locale: en\nidentity: {{name: Tester, email: tester@example.com}}\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\n    concept_policy: supported-new\nexecution:\n  work: {{max_input_bytes: 262144, max_result_bytes: 32768}}\n", serde_json::to_string(&root).unwrap())).unwrap();
        let (config, config_revision) =
            lk_core::config::Config::load_with_revision(&config_path).unwrap();
        let text = "Candidate evidence retains attribution and conditions.\n".repeat(1800);
        let target = "wiki/documents/source.md";
        let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "ext",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                rss_original: None,
                text: text.clone(),
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
        write(
            &root,
            target,
            &format!(
                "---\ntype: document\ntitle: Source\nllm_inputs:\n  concepts: {}\n---\n## Content\n{text}\n\n## Related Concepts\n",
                task.cache_hash
            ),
        );
        write(
            &root,
            "wiki/concepts/candidate.md",
            "---\ntype: concept\ntitle: Candidate\n---\n## Synthesis\nCandidate scope and conditions.\n",
        );
        write(
            &root,
            ".lorekeeper/queue/fixture.jsonl",
            &(serde_json::to_string(&task).unwrap() + "\n"),
        );
        let queries = [lk_core::discovery::DiscoveryQuery {
            query: "Candidate".into(),
            phrase: true,
            spacing: false,
        }];
        let screen =
            || screening(&root, &config, &config_revision, &task.task_id, &queries).unwrap();
        let ScreeningPreparation::Source { work, mut prepared } = screen() else {
            panic!("large raw source requires bounded observations");
        };
        let session = MutationSession::acquire(&root).unwrap();
        discovery::record(
            &session,
            &task.task_id,
            discovery::DiscoveryRequest {
                target_revision: file_revision(
                    &std::fs::read_to_string(root.join(target)).unwrap(),
                ),
                queries: queries.to_vec(),
            },
        )
        .unwrap();
        let work = persist_prepared(&session, &config, &config_revision, &mut prepared)
            .map(|persisted| {
                assert_eq!(persisted, *work);
                persisted
            })
            .unwrap();
        assert_eq!(
            work.plan.max_part_characters,
            Some(lk_core::semantic::INSPECTION_CHARACTERS)
        );
        assert!(work.plan.parts.len() > 1);
        let before = work_snapshot(&root, &work.plan.id);
        assert_eq!(
            persist_prepared(&session, &config, &config_revision, &mut prepared).unwrap(),
            work
        );
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
        assert!(matches!(screen(), ScreeningPreparation::Source { .. }));

        let observe = |summary: &str| {
            let mut last = None;
            loop {
                let (progress, request) =
                    work.prepare_inspection(&root, &prepared.resources).unwrap();
                let Some(request) = request.filter(|_| !progress.complete) else {
                    break;
                };
                let output = WorkOutput {
                    work_id: work.plan.id.clone(),
                    node_id: request["node_id"].as_str().unwrap().into(),
                    input_revision: request["input_revision"].as_str().unwrap().into(),
                    summary: summary.into(),
                    findings: vec![],
                    source_questions: vec![],
                    unresolved: vec![],
                    resolutions: vec![],
                };
                work.record(&session, output.clone(), &prepared.resources)
                    .unwrap();
                last = Some(output);
            }
            last.unwrap()
        };
        let mut root_output = observe("Attributed source observation.");
        let ScreeningPreparation::Candidates(observed) = screen() else {
            panic!("complete source observations admit candidate screening");
        };
        assert!(observed.source_observations.is_some());
        let request = observed.selection.next_request(false).unwrap().unwrap();
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value["source"]["role"],
            "complete-bounded-source-observations"
        );
        let output = serde_json::from_value(json!({
            "task_id":value["task_id"],"request_revision":value["request_revision"],
            "decisions":value["candidates"].as_array().unwrap().iter().map(|candidate| json!({
                "candidate_key":candidate["candidate_key"],"decision":"inspect",
                "reason":"Card-scoped assessment.",
            })).collect::<Vec<_>>(),
        }))
        .unwrap();
        session
            .apply(selection::plan_record(&root, &request, output).unwrap())
            .unwrap();
        let read = lk_vault::ReadControl::default();
        let mut reviews =
            lk_vault::ReviewInventory::load_controlled(&root, &config.vault.dirs, &read).unwrap();
        let ScreeningPreparation::Candidates(decided) = screen() else {
            panic!("recorded decisions keep the observed source");
        };
        let admitted = admitted_task(&root, &config, &decided, &mut reviews, &read).unwrap();
        assert!(!admitted.context["purpose"]["source_observations"].is_null());
        let binding = decided.selection.binding().unwrap();
        let roster = admitted.roster.as_ref().unwrap();
        assert_eq!(roster.selection, binding);
        assert_eq!(roster.queries(), queries);
        let evidence = resources::refresh_controlled(
            &root,
            &config,
            &config_revision,
            &admitted.context,
            &read,
        )
        .unwrap();
        assert!(
            evidence.roster.is_none(),
            "reading evidence consults no candidate roster"
        );
        assert_eq!(
            evidence.revision(&config_revision).unwrap(),
            admitted.revision(&config_revision).unwrap()
        );
        let RosterAdmission::Bound(bound) =
            with_current_roster(&root, &config, &config_revision, &evidence, &read).unwrap()
        else {
            panic!("the decided roster admits exactly this evidence");
        };
        assert_eq!(bound.roster.as_ref().unwrap().selection, binding);
        let exact = evidence.read_as(resources::ContextReading::Exact).unwrap();
        assert_ne!(exact.resources, evidence.resources);
        let RosterAdmission::Bound(bound) =
            with_current_roster(&root, &config, &config_revision, &exact, &read).unwrap()
        else {
            panic!("evidence read as exact bytes is admitted by the same roster");
        };
        assert_eq!(bound.reading(), resources::ContextReading::Exact);
        let later = "wiki/concepts/later.md";
        write(
            &root,
            later,
            "---\ntype: concept\ntitle: Later\n---\n## Synthesis\nCandidate scope added later.\n",
        );
        assert!(
            matches!(
                with_current_roster(&root, &config, &config_revision, &evidence, &read).unwrap(),
                RosterAdmission::Changed(reason) if reason.contains("1 unknown")
            ),
            "an undecided candidate leaves the evidence readable but admits no result"
        );
        assert_eq!(
            resources::refresh_controlled(
                &root,
                &config,
                &config_revision,
                &admitted.context,
                &read,
            )
            .unwrap()
            .revision(&config_revision)
            .unwrap(),
            admitted.revision(&config_revision).unwrap()
        );
        std::fs::remove_file(root.join(later)).unwrap();
        let mut plan = MutationPlan::default();
        roster.seal.plan(&root, &mut plan).unwrap();
        session.apply(plan).unwrap();
        let sealed = || {
            sealed_task_controlled(
                &root,
                &config,
                &config_revision,
                &admitted,
                &queries,
                &binding,
                None,
                &read,
            )
        };
        let evidence = sealed().unwrap();
        assert_eq!(
            evidence.prepared.revision(&config_revision).unwrap(),
            admitted.revision(&config_revision).unwrap()
        );
        assert!(!evidence.observation_guards.is_empty());
        assert!(
            evidence
                .observation_guards
                .keys()
                .all(|path| !evidence.prepared.pages.contains_key(path))
        );

        root_output.summary = "Corrected source observation.".into();
        work.record(&session, root_output, &prepared.resources)
            .unwrap();
        assert_eq!(
            sealed()
                .err()
                .unwrap()
                .downcast_ref::<super::super::queue::application::PublicationRejection>()
                .map(|rejection| rejection.kind),
            Some(RejectionKind::SemanticContextChanged)
        );
        let ScreeningPreparation::Candidates(changed) = screen() else {
            panic!("a corrected observation remains a complete source");
        };
        let progress = changed.selection.progress();
        assert_eq!(
            progress.unknown, progress.candidates,
            "decisions made against another source do not carry over"
        );
    }
    #[cfg(unix)]
    #[test]
    fn native_inspection_admission_checks_only_the_next_exact_request() {
        for partition in [state::Partition::Bytes, state::Partition::Inspection] {
            check_native_inspection_admission(partition);
        }
    }

    #[cfg(unix)]
    fn check_native_inspection_admission(partition: state::Partition) {
        for (index, current_contract) in [(0, true), (1, true), (0, false), (1, false)] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let text = "Source-scoped evidence.\n\n".repeat(1000);
            let resources = vec![lk_core::semantic::SemanticResource {
                address: "wiki/documents/source.md".into(),
                revision: file_revision(&text),
                authority: lk_core::semantic::SemanticAuthority::Page,
                text,
            }];
            let work = Work::build_partitioned(
                &resources,
                "subject",
                json!({"purpose":{"kind":"synthesis","locale":"en"}}),
                WorkLimits {
                    max_input_bytes: 8192,
                    max_result_bytes: 2048,
                },
                partition,
            )
            .unwrap()
            .persist(&session, lk_vault::MutationPlan::default())
            .unwrap();
            let (_, requests) = work
                .prepare_inspections(root.path(), &resources, 2)
                .unwrap();
            assert_eq!(requests.len(), 2);
            let contract = if current_contract {
                super::super::execution::observation::contract_revision(&requests[index]).unwrap()
            } else {
                file_revision("older-native-contract")
            };
            let mutation = work
                .plan_inspection_action(
                    root.path(),
                    &resources,
                    &requests[index],
                    &contract,
                    "Explicit source-scoped action required.".into(),
                )
                .unwrap();
            session.apply(mutation).unwrap();
            let action = root
                .path()
                .join(".lorekeeper/work")
                .join(&work.plan.id)
                .join("inspection-actions.json");
            let before = std::fs::read(&action).unwrap();
            assert_eq!(
                inspection_admission(root.path(), &work, &resources)
                    .unwrap()
                    .is_some(),
                index == 0 && (current_contract || partition == state::Partition::Bytes)
            );
            if index == 1 {
                work.record(
                    &session,
                    WorkOutput {
                        work_id: work.plan.id.clone(),
                        node_id: requests[0]["node_id"].as_str().unwrap().into(),
                        input_revision: requests[0]["input_revision"].as_str().unwrap().into(),
                        summary: "The exact assigned source evidence was read.".into(),
                        findings: vec![],
                        source_questions: vec![],
                        unresolved: vec![],
                        resolutions: vec![],
                    },
                    &resources,
                )
                .unwrap();
                assert_eq!(
                    inspection_admission(root.path(), &work, &resources)
                        .unwrap()
                        .is_some(),
                    current_contract || partition == state::Partition::Bytes
                );
                assert_eq!(work.progress(root.path(), &resources).unwrap().completed, 1);
            }
            assert_eq!(std::fs::read(&action).unwrap(), before);
            std::fs::write(&action, "{}").unwrap();
            assert!(inspection_admission(root.path(), &work, &resources).is_err());
            assert_eq!(std::fs::read(action).unwrap(), b"{}");
        }
    }

    fn completed_audit_work(root: &Path) -> (Work, Prepared, Value) {
        completed_audit_work_with_configuration(root, false)
    }

    fn completed_audit_work_with_configuration(
        root: &Path,
        legacy: bool,
    ) -> (Work, Prepared, Value) {
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
            &if legacy {
                prepared.legacy_revision(&config_revision).unwrap()
            } else {
                prepared.revision(&config_revision).unwrap()
            },
            prepared.context.clone(),
            limits(&config).unwrap(),
        )
        .unwrap();
        assert!(work.plan.parts.len() > 1);
        record_every_inspection(root, &session, &work, &prepared.resources);
        let result = reviewed_assessment(&prepared);
        (work, prepared, result)
    }

    fn record_every_inspection(
        root: &Path,
        session: &MutationSession,
        work: &Work,
        resources: &[lk_core::semantic::SemanticResource],
    ) {
        loop {
            let (status, request) = work.prepare_inspection(root, resources).unwrap();
            if status.complete {
                break;
            }
            let request = request.unwrap();
            work.record(
                session,
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
                resources,
            )
            .unwrap();
        }
    }

    fn reviewed_assessment(prepared: &Prepared) -> Value {
        let Subject::Audit {
            path,
            file_revision,
            input_revision,
        } = &prepared.subject
        else {
            unreachable!()
        };
        json!({"assessments":[{
            "path":path,"file_revision":file_revision,"input_revision":input_revision,
            "disposition":"reviewed","reason":"Fixture completed the exact selected reads.","reviews":[],
        }]})
    }

    #[test]
    fn saved_work_preserves_exact_observations_and_legacy_configuration_boundaries() {
        for legacy in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let (work, _, _) = completed_audit_work_with_configuration(&root, legacy);
            let config_path = root.join("config.yaml");
            let original = std::fs::read_to_string(&config_path).unwrap();
            let (config, revision) =
                lk_core::config::Config::load_with_revision(&config_path).unwrap();
            let (loaded, prepared) = load(&root, &config, &revision, &work.plan.id).unwrap();
            assert_eq!(loaded, work);
            assert!(
                loaded
                    .progress(&root, &prepared.resources)
                    .unwrap()
                    .complete
            );
            let before = std::fs::read(
                root.join(".lorekeeper/work")
                    .join(&work.plan.id)
                    .join("plan.json"),
            )
            .unwrap();
            let changed = original.replace(
                "max_input_bytes: 8192",
                "max_calls: 9, max_parallel: 3, max_screening_batches: 1, max_input_bytes: 8192",
            );
            std::fs::write(&config_path, changed).unwrap();
            let (config, revision) =
                lk_core::config::Config::load_with_revision(&config_path).unwrap();
            let resumed = load(&root, &config, &revision, &work.plan.id);
            if legacy {
                assert!(
                    resumed.is_err(),
                    "legacy work cannot guess its original configuration"
                );
            } else {
                let (loaded, mut prepared) = resumed.unwrap();
                assert_eq!(loaded, work);
                assert!(
                    loaded
                        .progress(&root, &prepared.resources)
                        .unwrap()
                        .complete
                );
                let session = MutationSession::acquire(&root).unwrap();
                assert_eq!(
                    persist_prepared(&session, &config, &revision, &mut prepared).unwrap(),
                    work
                );
            }
            assert_eq!(
                std::fs::read(
                    root.join(".lorekeeper/work")
                        .join(&work.plan.id)
                        .join("plan.json")
                )
                .unwrap(),
                before
            );
            std::fs::write(&config_path, original).unwrap();
            let (config, revision) =
                lk_core::config::Config::load_with_revision(&config_path).unwrap();
            assert_eq!(
                load(&root, &config, &revision, &work.plan.id).unwrap().0,
                work
            );
            assert_eq!(
                std::fs::read_dir(root.join(".lorekeeper/work"))
                    .unwrap()
                    .count(),
                1
            );
        }
    }

    /// A configuration bounding Work input and result bytes.
    fn bounded_config(
        root: &Path,
        max_input_bytes: usize,
        max_result_bytes: usize,
    ) -> (lk_core::config::Config, String) {
        let config_path = root.join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "vault:\n  root: {}\n  locale: en\nidentity: {{name: Tester, email: tester@example.com}}\nsources:\n  notes:\n    type: manual\n    params: {{inbox_dir: inbox}}\nexecution:\n  work: {{max_input_bytes: {max_input_bytes}, max_result_bytes: {max_result_bytes}}}\n",
                serde_json::to_string(root).unwrap()
            ),
        )
        .unwrap();
        lk_core::config::Config::load_with_revision(&config_path).unwrap()
    }

    #[cfg(unix)]
    const CONTEXT_RELATED: &str = "wiki/concepts/related.md";
    #[cfg(unix)]
    const CONTEXT_RELATED_RAW: &str = "---\ntype: concept\ntitle: Related\nsource_count: 1\n---\n## Synthesis\nA related claim.\n";
    #[cfg(unix)]
    const CONTEXT_SUBJECT: &str = "wiki/documents/subject.md";

    /// An audit subject too long for one part that relates a page with bookkeeping, so its
    /// exact bytes and semantic reading differ.
    #[cfg(unix)]
    fn context_audit_fixture(root: &Path) -> (lk_core::config::Config, String) {
        write(root, CONTEXT_RELATED, CONTEXT_RELATED_RAW);
        write(
            root,
            CONTEXT_SUBJECT,
            &format!(
                "---\ntype: document\ntitle: Subject\n---\n## Content\n[Related](../concepts/related.md)\n\n{}",
                "A source-scoped observation.\n\n".repeat(500)
            ),
        );
        bounded_config(root, 8192, 2048)
    }

    #[cfg(unix)]
    #[test]
    fn work_planned_on_exact_context_bytes_resumes_with_its_observations_and_holds() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (config, revision) = context_audit_fixture(&root);
        let (related, related_raw, path) = (CONTEXT_RELATED, CONTEXT_RELATED_RAW, CONTEXT_SUBJECT);
        let current = resources::audit(&root, &config, path).unwrap();
        let exact = current.read_as(resources::ContextReading::Exact).unwrap();
        assert_ne!(
            exact.resources, current.resources,
            "the related page reads differently as exact bytes"
        );
        let session = MutationSession::acquire(&root).unwrap();
        let work = Work::create(
            &session,
            &exact.resources,
            &exact.revision(&revision).unwrap(),
            exact.context.clone(),
            limits(&config).unwrap(),
        )
        .unwrap();
        let (_, request) = work.prepare_inspection(&root, &exact.resources).unwrap();
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
            &exact.resources,
        )
        .unwrap();
        drop(session);
        let (_, next) = work.prepare_inspection(&root, &exact.resources).unwrap();
        let held = next.unwrap()["node_id"].as_str().unwrap().to_owned();
        update_inspection_attempt(
            &root,
            &super::super::ConfigurationPin::Absent,
            &config,
            &revision,
            &work.plan.id,
            &held,
            state::AttemptUpdate::Hold("Operator review before another invocation.".into()),
            false,
        )
        .unwrap();

        let mut resumed = resources::audit(&root, &config, path).unwrap();
        assert_eq!(
            expected(&root, &mut resumed, &config, &revision).unwrap(),
            work,
            "unchanged pages resume the Work, its observations and its holds"
        );
        assert_eq!(
            work.progress(&root, &resumed.resources).unwrap().completed,
            1
        );
        assert!(
            root.join(".lorekeeper/work")
                .join(&work.plan.id)
                .join("inspection-timeouts.json")
                .exists()
        );
        assert_eq!(
            load(&root, &config, &revision, &work.plan.id)
                .unwrap()
                .1
                .reading(),
            resources::ContextReading::Exact,
            "the named Work loads in the reading it was planned under"
        );

        write(
            &root,
            related,
            &related_raw.replace("source_count: 1", "source_count: 2"),
        );
        let mut cited = resources::audit(&root, &config, path).unwrap();
        assert_ne!(
            expected(&root, &mut cited, &config, &revision).unwrap(),
            work,
            "an exact-bytes Work cannot resume once a page it read changed"
        );
        assert_eq!(
            cited.revision(&revision).unwrap(),
            current.revision(&revision).unwrap(),
            "the current reading ignores the citation update"
        );
        let session = MutationSession::acquire(&root).unwrap();
        let semantic = persist_prepared(&session, &config, &revision, &mut cited).unwrap();
        drop(session);
        assert_ne!(semantic, work);

        write(&root, related, related_raw);
        let mut reverted = resources::audit(&root, &config, path).unwrap();
        assert_eq!(
            expected(&root, &mut reverted, &config, &revision).unwrap(),
            semantic,
            "restored bytes leave the newer semantic Work selected"
        );
        assert_eq!(reverted.reading(), resources::ContextReading::Semantic);
        assert_eq!(
            load(&root, &config, &revision, &semantic.plan.id)
                .unwrap()
                .1
                .reading(),
            resources::ContextReading::Semantic
        );
        assert!(
            super::super::queue::application::is_semantic_context_changed(
                &load(&root, &config, &revision, &work.plan.id)
                    .err()
                    .unwrap()
            ),
            "the superseded exact-bytes Work is no longer the one this subject continues"
        );
    }

    #[cfg(unix)]
    #[test]
    fn completed_exact_context_audit_work_publishes_through_its_reading() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (config, revision) = context_audit_fixture(&root);
        let exact = resources::audit(&root, &config, CONTEXT_SUBJECT)
            .unwrap()
            .read_as(resources::ContextReading::Exact)
            .unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        let work = Work::create(
            &session,
            &exact.resources,
            &exact.revision(&revision).unwrap(),
            exact.context.clone(),
            limits(&config).unwrap(),
        )
        .unwrap();
        record_every_inspection(&root, &session, &work, &exact.resources);
        let result = reviewed_assessment(&exact);
        finish(&session, &work, &exact, result.clone()).unwrap();
        drop(session);
        let assessment = serde_json::from_value(result["assessments"][0].clone()).unwrap();
        let mut plan = MutationPlan::default();
        assert!(
            guard_audit_controlled(
                &root,
                &config,
                &revision,
                &assessment,
                &mut plan,
                &lk_vault::ReadControl::default(),
            )
            .unwrap(),
            "the assessment publishes as the completed exact-bytes Work"
        );
        let steps = serde_json::to_value(&plan).unwrap()["steps"].clone();
        let steps = steps.as_array().unwrap();
        let receipt = Path::new(".lorekeeper/work")
            .join(&work.plan.id)
            .join("publication.json");
        assert!(
            steps.iter().any(|step| step["kind"] == "create"
                && Path::new(step["path"].as_str().unwrap()) == receipt),
            "publication journals that Work's receipt"
        );
        assert!(
            steps.iter().any(|step| step["kind"] == "edit"
                && step["path"] == CONTEXT_RELATED
                && step["before"] == CONTEXT_RELATED_RAW
                && step["after"] == CONTEXT_RELATED_RAW),
            "publication guards the related page's exact bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_audit_whose_linked_page_changed_replans_and_reuses_its_unchanged_inspections() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (config, revision) = context_audit_fixture(&root);
        let session = MutationSession::acquire(&root).unwrap();
        let mut prepared = resources::audit(&root, &config, CONTEXT_SUBJECT).unwrap();
        let work = persist_prepared(&session, &config, &revision, &mut prepared).unwrap();
        assert!(work.plan.parts.len() > 1);
        loop {
            let (_, request) = work.prepare_inspection(&root, &prepared.resources).unwrap();
            let Some(request) = request.filter(|request| request["kind"] == "inspect") else {
                break;
            };
            let output = WorkOutput {
                work_id: work.plan.id.clone(),
                node_id: request["node_id"].as_str().unwrap().into(),
                input_revision: request["input_revision"].as_str().unwrap().into(),
                summary: "The assigned evidence was read.".into(),
                findings: vec![],
                source_questions: vec![],
                unresolved: vec![],
                resolutions: vec![],
            };
            work.record(&session, output, &prepared.resources).unwrap();
        }
        assert_eq!(
            work.progress(&root, &prepared.resources).unwrap().completed,
            work.plan.parts.len()
        );
        write(
            &root,
            CONTEXT_RELATED,
            &CONTEXT_RELATED_RAW.replace("A related claim.", "A corrected related claim."),
        );
        // The Work read the linked page as it was: it no longer publishes.
        let error = resources::refresh_controlled(
            &root,
            &config,
            &revision,
            &work.context,
            &lk_vault::ReadControl::default(),
        )
        .err()
        .unwrap();
        assert!(matches!(
            error.downcast_ref::<crate::commands::queue::application::PublicationRejection>(),
            Some(rejection)
                if rejection.kind == crate::commands::queue::application::RejectionKind::SemanticContextChanged
        ));
        // Replanned, only the inspections holding the changed page are read again.
        let mut replanned = resources::audit(&root, &config, CONTEXT_SUBJECT).unwrap();
        let new = persist_prepared(&session, &config, &revision, &mut replanned).unwrap();
        assert_ne!(new.plan.id, work.plan.id);
        let moved = new
            .plan
            .parts
            .iter()
            .filter(|part| !work.plan.parts.iter().any(|prior| prior.id == part.id))
            .count();
        assert!(moved > 0 && moved < new.plan.parts.len());
        assert_eq!(
            new.progress(&root, &replanned.resources).unwrap().completed,
            new.plan.parts.len() - moved
        );
    }

    #[cfg(unix)]
    #[test]
    fn work_planned_before_page_alignment_continues_under_its_byte_filled_parts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (config, revision) = context_audit_fixture(&root);
        let mut prepared = resources::audit(&root, &config, CONTEXT_SUBJECT).unwrap();
        let current = expected(&root, &mut prepared, &config, &revision).unwrap();
        assert_eq!(current.plan.policy, lk_core::semantic::INSPECTION_POLICY);
        let session = MutationSession::acquire(&root).unwrap();
        let byte_filled = Work::build_partitioned(
            &prepared.resources,
            &prepared.revision(&revision).unwrap(),
            prepared.context.clone(),
            limits(&config).unwrap(),
            state::Partition::LegacyInspection,
        )
        .unwrap()
        .persist(&session, lk_vault::MutationPlan::default())
        .unwrap();
        drop(session);
        assert_eq!(
            byte_filled.plan.policy,
            lk_core::semantic::LEGACY_INSPECTION_POLICY
        );
        assert_eq!(
            expected(&root, &mut prepared, &config, &revision).unwrap(),
            byte_filled
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_whole_audit_capture_changes_once_a_stored_work_continues_its_subject() {
        // A semantic Work keeps the revision and is told apart by the stored Work; a revived
        // exact-bytes Work also changes the reading.
        for reading in [
            resources::ContextReading::Semantic,
            resources::ContextReading::Exact,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let (config, revision) = context_audit_fixture(&root);
            let pages = lk_graph::scan::scan_vault(&root, false).unwrap();
            let candidate = lk_graph::audit::plan_selected(
                &root,
                &config.vault.dirs,
                lk_graph::audit::AuditCorpus::from(&config.graph.audit),
                &pages,
                1,
                &[CONTEXT_SUBJECT.into()],
                u64::MAX,
            )
            .unwrap()
            .candidates
            .remove(0);
            let read = lk_vault::ReadControl::default();
            let capture =
                || audit::capture_controlled(&root, &config, &revision, &candidate, &read).unwrap();
            let whole = capture();
            assert!(whole.same_input(&capture()));
            let stored = resources::audit_candidate(&root, &config, &candidate)
                .unwrap()
                .read_as(reading)
                .unwrap();
            let session = MutationSession::acquire(&root).unwrap();
            Work::create(
                &session,
                &stored.resources,
                &stored.revision(&revision).unwrap(),
                stored.context.clone(),
                limits(&config).unwrap(),
            )
            .unwrap();
            drop(session);
            assert!(
                !whole.same_input(&capture()),
                "{reading:?}: a whole judgment cannot complete the stored Work now continuing the subject"
            );
        }
    }

    #[test]
    fn a_reading_beyond_the_input_bound_leaves_the_other_reading_to_continue() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        // The escape reads as ASCII bytes exactly and as one four-byte scalar semantically, so
        // a bound can admit every exact scalar but not that one.
        let related = "wiki/concepts/a-related-concept-whose-address-outgrows-its-subject.md";
        write(
            &root,
            related,
            "---\ntype: concept\ntitle: \"\\U0001F600\"\n---\nR.\n",
        );
        let subject = "wiki/documents/s.md";
        write(
            &root,
            subject,
            &format!(
                "---\ntype: document\ntitle: {}\n---\n[R](../concepts/a-related-concept-whose-address-outgrows-its-subject.md)\n",
                // A long title raises the smallest admissible bound into a valid configuration.
                "T".repeat(3000)
            ),
        );
        let (config, _) = bounded_config(&root, 8192, 1024);
        let current = resources::audit(&root, &config, subject).unwrap();
        let exact = current.read_as(resources::ContextReading::Exact).unwrap();
        let plans = |prepared: &Prepared, max_input_bytes| {
            Work::build_partitioned(
                &prepared.resources,
                &prepared.revision("configuration").unwrap(),
                prepared.context.clone(),
                WorkLimits {
                    max_input_bytes,
                    max_result_bytes: 1024,
                },
                state::Partition::Inspection,
            )
            .is_ok()
        };
        let (mut low, mut high) = (1, 1 << 20);
        while low < high {
            let middle = (low + high) / 2;
            if plans(&exact, middle) {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        assert!(
            low >= 4096,
            "configuration admits no bound below 4096 bytes"
        );
        assert!(
            !plans(&current, low),
            "the semantic reading needs more than {low} bytes"
        );
        let (config, revision) = bounded_config(&root, low, 1024);
        let mut current = resources::audit(&root, &config, subject).unwrap();
        let exact = current.read_as(resources::ContextReading::Exact).unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        let work = Work::create(
            &session,
            &exact.resources,
            &exact.revision(&revision).unwrap(),
            exact.context.clone(),
            limits(&config).unwrap(),
        )
        .unwrap();
        drop(session);
        assert_eq!(
            expected(&root, &mut current, &config, &revision).unwrap(),
            work
        );
        assert_eq!(current.reading(), resources::ContextReading::Exact);
    }

    #[test]
    fn queue_results_are_checked_against_the_work_their_task_continues() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.yaml");
        std::fs::write(&config_path, "vault:\n  root: vault\n  locale: en\nidentity:\n  name: Tester\n  email: tester@example.com\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n").unwrap();
        let (mut config, config_revision) =
            lk_core::config::Config::load_with_revision(&config_path).unwrap();
        // Full-context extraction reads every match without screening, so context reports
        // ready scope for the supplied recipes.
        config.execution.work.extraction_context =
            lk_core::execution::ExtractionContext::FullContext;
        std::fs::create_dir_all(directory.path().join("vault")).unwrap();
        let root = directory.path().join("vault").canonicalize().unwrap();
        let target = "wiki/documents/source.md";
        let owner = "wiki/concepts/canonical.md";
        let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "ext",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                rss_original: None,
                text: "Selected Candidate has source-scoped evidence.".into(),
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
        let source = format!(
            "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nSelected Candidate has attributed source material.\n\n## Related Concepts\n",
            task.cache_hash
        );
        write(&root, target, &source);
        let owner_raw = "---\ntype: concept\ntitle: Selected Candidate\nsource_count: 1\n---\n## Synthesis\nExisting scoped interpretation.\n";
        write(&root, owner, owner_raw);
        write(
            &root,
            ".lorekeeper/queue/fixture.jsonl",
            &serde_json::to_string(&task).unwrap(),
        );
        let discovery = vec![lk_core::discovery::DiscoveryQuery {
            query: "Selected Candidate".into(),
            phrase: true,
            spacing: false,
        }];
        let drafted = |prepared: &Prepared| {
            serde_json::to_value(lk_queue::TaskResult {
                selection: None,
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
                target_revision: file_revision(&source),
                context_revision: prepared.revision(&config_revision).unwrap(),
                discovery: discovery.clone(),
                target: task.target.clone(),
                date: jiff::civil::date(2026, 10, 20),
                concepts: vec![],
            })
            .unwrap()
        };
        let apply = |result| {
            prepare_queue_publication(&root, &config, &config_revision, &task.task_id, result)
        };
        let semantic =
            resources::task_with_discovery(&root, &config, &task.task_id, &discovery).unwrap();
        assert!(
            semantic
                .resources
                .iter()
                .any(|resource| resource.address == owner
                    && resource.authority == lk_core::semantic::SemanticAuthority::Context)
        );
        let exact = semantic.read_as(resources::ContextReading::Exact).unwrap();
        assert_ne!(
            exact.revision(&config_revision).unwrap(),
            semantic.revision(&config_revision).unwrap()
        );
        let session = MutationSession::acquire(&root).unwrap();
        Work::create(
            &session,
            &exact.resources,
            &exact.revision(&config_revision).unwrap(),
            exact.context.clone(),
            limits(&config).unwrap(),
        )
        .unwrap();
        drop(session);
        let context = task_context(
            &root,
            &config,
            &config_revision,
            &task.task_id,
            Some(&discovery),
            None,
        )
        .unwrap();
        assert_eq!(context["part_count"], 1);
        assert_eq!(
            context["requires_bounded_work"], true,
            "a stored Work continues through the Work protocol, however few its parts"
        );
        let rejection = |result| {
            apply(result)
                .err()
                .unwrap()
                .downcast_ref::<super::super::queue::application::PublicationRejection>()
                .map(|rejection| rejection.kind)
        };
        assert_eq!(
            rejection(drafted(&exact)),
            Some(RejectionKind::EvidenceScopeChanged),
            "a result drafted from the stored exact-bytes Work reaches that Work's completion check"
        );
        assert_eq!(
            rejection(drafted(&semantic)),
            Some(RejectionKind::SemanticContextChanged),
            "the task continues its exact-bytes Work"
        );

        write(
            &root,
            owner,
            &owner_raw.replace("source_count: 1", "source_count: 2"),
        );
        assert!(
            apply(drafted(&semantic)).is_ok(),
            "once a page it read changes, the semantic reading continues as whole input"
        );
        assert_eq!(
            rejection(drafted(&exact)),
            Some(RejectionKind::SemanticContextChanged)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_selected_request_is_a_changed_scope_once_a_stored_work_continues_its_task() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (mut config, config_revision) = bounded_config(&root, 8192, 2048);
        config.execution.work.extraction_context =
            lk_core::execution::ExtractionContext::FullContext;
        let target = "wiki/documents/source.md";
        let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "ext",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                rss_original: None,
                text: "Selected Candidate has source-scoped evidence.".into(),
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
        write(
            &root,
            target,
            &format!(
                "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nSelected Candidate has attributed source material.\n\n## Related Concepts\n",
                task.cache_hash
            ),
        );
        // The comment fills several parts as exact bytes and vanishes from the semantic reading.
        write(
            &root,
            "wiki/concepts/canonical.md",
            &format!(
                "---\ntype: concept\ntitle: Selected Candidate\n{}---\n## Synthesis\nExisting scoped interpretation.\n",
                "# An authored comment the parsed frontmatter does not keep.\n".repeat(400)
            ),
        );
        write(
            &root,
            ".lorekeeper/queue/fixture.jsonl",
            &serde_json::to_string(&task).unwrap(),
        );
        let discovery = vec![lk_core::discovery::DiscoveryQuery {
            query: "Selected Candidate".into(),
            phrase: true,
            spacing: false,
        }];
        let mut prepared =
            resources::task_with_discovery(&root, &config, &task.task_id, &discovery).unwrap();
        let work = expected(&root, &mut prepared, &config, &config_revision).unwrap();
        assert_eq!(work.plan.parts.len(), 1);
        let owner = lk_vault::resolve_page_owner(&root, Path::new(target)).unwrap();
        let input = native::selected_task(&config, &task, &prepared, &work, &owner).unwrap();
        let read = lk_vault::ReadControl::default();
        let current = || {
            generation::current_selected(&root, &config, &config_revision, &input, &read).unwrap()
        };
        assert!(current().is_some());
        let exact = prepared.read_as(resources::ContextReading::Exact).unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        let stored = Work::create(
            &session,
            &exact.resources,
            &exact.revision(&config_revision).unwrap(),
            exact.context.clone(),
            limits(&config).unwrap(),
        )
        .unwrap();
        drop(session);
        assert!(stored.plan.parts.len() > 1);
        assert!(
            current().is_none(),
            "the task now continues its stored exact-bytes Work instead of the selected request"
        );
    }

    #[test]
    fn empty_stage_mapping_resumes_current_policy_work_without_rewriting_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, _) = completed_audit_work_with_configuration(&root, false);
        let artifacts = || {
            walkdir::WalkDir::new(root.join(".lorekeeper/work"))
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| {
                    (
                        entry.path().to_owned(),
                        std::fs::read(entry.path()).unwrap(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = artifacts();
        let result = work.result(&root, &prepared.resources).unwrap();
        let path = root.join("config.yaml");
        let original = std::fs::read_to_string(&path).unwrap();
        let changed = original.replace("execution:\n", "execution:\n  stages: {}\n");
        assert_ne!(original, changed);
        std::fs::write(&path, changed).unwrap();
        let (config, revision) = lk_core::config::Config::load_with_revision(&path).unwrap();
        let (loaded, prepared) = load(&root, &config, &revision, &work.plan.id).unwrap();
        assert_eq!(loaded, work);
        assert_eq!(loaded.result(&root, &prepared.resources).unwrap(), result);
        assert_eq!(artifacts(), before);
    }

    fn work_snapshot(root: &Path, id: &str) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let directory = root.join(".lorekeeper/work").join(id);
        walkdir::WalkDir::new(directory)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    entry.path().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn published_completion_refuses_changed_result_and_keeps_exact_retry_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let session = MutationSession::acquire(&root).unwrap();
        finish(&session, &work, &prepared, result.clone()).unwrap();
        let mut publication = MutationPlan::default();
        work.plan_publication(&mut publication, &root, &result)
            .unwrap();
        session.apply(publication).unwrap();
        drop(session);
        let input = tempfile::NamedTempFile::new().unwrap();
        let opts = GlobalOptions {
            config: Some(root.join("config.yaml")),
            template_dir: None,
        };
        let command = || WorkCommand::Finish {
            work_id: work.plan.id.clone(),
            file: input.path().to_owned(),
            prepared: None,
        };
        let before = work_snapshot(&root, &work.plan.id);
        let mut changed = result.clone();
        changed["assessments"][0]["reason"] = json!("A different published explanation.");
        std::fs::write(input.path(), serde_json::to_string(&changed).unwrap()).unwrap();
        assert!(run(&opts, command()).await.is_err());
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
        for path in before.keys() {
            if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("seal.json" | "final.json" | "assessment.json" | "publication.json")
            ) {
                let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
            }
        }
        let before = work_snapshot(&root, &work.plan.id);
        std::fs::write(input.path(), serde_json::to_string(&result).unwrap()).unwrap();
        run(&opts, command()).await.unwrap();
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
        verify_finished_prepared(&root, &work, &prepared).unwrap();
    }

    #[tokio::test]
    async fn published_observations_refuse_replacement_and_preserve_exact_retries() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let session = MutationSession::acquire(&root).unwrap();
        finish(&session, &work, &prepared, result.clone()).unwrap();
        let mut publication = MutationPlan::default();
        work.plan_publication(&mut publication, &root, &result)
            .unwrap();
        session.apply(publication).unwrap();
        drop(session);
        let path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("nodes")
            .join(format!("{}.json", work.plan.parts[0].id));
        let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let original: WorkOutput = serde_json::from_value(stored["output"].clone()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&stored).unwrap()).unwrap();
        let before = work_snapshot(&root, &work.plan.id);
        let input = tempfile::NamedTempFile::new().unwrap();
        let opts = GlobalOptions {
            config: Some(root.join("config.yaml")),
            template_dir: None,
        };
        let command = || WorkCommand::Record {
            file: input.path().to_owned(),
        };
        let mut changed = original.clone();
        changed.summary.push_str(" A later interpretation.");
        std::fs::write(input.path(), serde_json::to_string(&changed).unwrap()).unwrap();
        assert!(run(&opts, command()).await.is_err());
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
        std::fs::write(input.path(), serde_json::to_string(&original).unwrap()).unwrap();
        run(&opts, command()).await.unwrap();
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
        assert!(work.progress(&root, &prepared.resources).unwrap().complete);
        verify_finished_prepared(&root, &work, &prepared).unwrap();
    }

    #[test]
    fn unpublished_completion_update_pins_publication_absence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let session = MutationSession::acquire(&root).unwrap();
        finish(&session, &work, &prepared, result.clone()).unwrap();
        let mut changed = result.clone();
        changed["assessments"][0]["reason"] = json!("A corrected unpublished explanation.");
        let (_, mutation) = plan_finish(&session, &work, &prepared, changed).unwrap();
        let mut publication = MutationPlan::default();
        work.plan_publication(&mut publication, &root, &result)
            .unwrap();
        session.apply(publication).unwrap();
        let before = work_snapshot(&root, &work.plan.id);
        assert!(session.apply(mutation).is_err());
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
    }

    #[cfg(unix)]
    #[test]
    fn finish_refuses_staged_fifo_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("assessment.json");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let session = MutationSession::acquire(&root).unwrap();
            send.send(plan_finish(&session, &work, &prepared, result).is_err())
                .unwrap();
        });
        assert!(
            receive
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("finish waited on a staged nonregular file")
        );
        worker.join().unwrap();
        use std::os::unix::fs::FileTypeExt;
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }

    #[test]
    fn finish_refuses_ambiguous_staged_result_without_replacing_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let session = MutationSession::acquire(&root).unwrap();
        finish(&session, &work, &prepared, result.clone()).unwrap();
        let path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("assessment.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let ambiguous = raw.replacen("\"reason\":", "\"reason\":\"Ambiguous\",\"reason\":", 1);
        assert_ne!(ambiguous, raw);
        std::fs::write(&path, ambiguous).unwrap();
        let before = work_snapshot(&root, &work.plan.id);
        assert!(finish(&session, &work, &prepared, result).is_err());
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
    }

    #[test]
    fn unpublished_completion_changes_atomically_and_pins_both_result_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let session = MutationSession::acquire(&root).unwrap();
        finish(&session, &work, &prepared, result.clone()).unwrap();
        let before = work_snapshot(&root, &work.plan.id);
        let mut changed = result;
        changed["assessments"][0]["reason"] = json!("A corrected unpublished explanation.");
        finish(&session, &work, &prepared, changed.clone()).unwrap();
        verify_finished_prepared(&root, &work, &prepared).unwrap();
        for (path, raw) in before {
            if !matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("seal.json" | "final.json" | "assessment.json")
            ) {
                assert_eq!(std::fs::read(path).unwrap(), raw);
            }
        }
        for artifact in ["final.json", "assessment.json"] {
            let (_, mutation) = plan_finish(&session, &work, &prepared, changed.clone()).unwrap();
            let path = root
                .join(".lorekeeper/work")
                .join(&work.plan.id)
                .join(artifact);
            let raw = std::fs::read_to_string(&path).unwrap();
            std::fs::write(&path, format!("{raw}\n")).unwrap();
            assert!(session.apply(mutation).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{raw}\n"));
            std::fs::write(path, raw).unwrap();
        }
    }

    #[test]
    fn finish_refuses_inconsistent_prior_completion_without_repair() {
        for corrupt in [
            "seal",
            "publication",
            "orphan-seal",
            "orphan-final",
            "stage",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let (work, prepared, result) = completed_audit_work(&root);
            let session = MutationSession::acquire(&root).unwrap();
            finish(&session, &work, &prepared, result.clone()).unwrap();
            let directory = root.join(".lorekeeper/work").join(&work.plan.id);
            match corrupt {
                "seal" => std::fs::write(directory.join("seal.json"), "{}").unwrap(),
                "publication" => std::fs::write(directory.join("publication.json"), "{}").unwrap(),
                "orphan-seal" => std::fs::remove_file(directory.join("final.json")).unwrap(),
                "orphan-final" => std::fs::remove_file(directory.join("seal.json")).unwrap(),
                "stage" => std::fs::write(directory.join("assessment.json"), "{}").unwrap(),
                _ => unreachable!(),
            }
            let before = work_snapshot(&root, &work.plan.id);
            assert!(
                finish(&session, &work, &prepared, result).is_err(),
                "{corrupt}"
            );
            assert_eq!(work_snapshot(&root, &work.plan.id), before);
            assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn finish_refuses_staged_symlink_before_reading_its_target() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let session = MutationSession::acquire(&root).unwrap();
        let path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("assessment.json");
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), [0xff]).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        let before = work_snapshot(&root, &work.plan.id);
        let error = plan_finish(&session, &work, &prepared, result)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("UTF-8"), "{error}");
        assert_eq!(std::fs::read(outside.path()).unwrap(), [0xff]);
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn standalone_finish_rejects_ambiguous_external_input_without_sealing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, _, result) = completed_audit_work(&root);
        let input = tempfile::NamedTempFile::new().unwrap();
        assert!(!input.path().starts_with(&root));
        let raw = serde_json::to_string(&result).unwrap();
        let ambiguous = raw.replacen("\"reason\":", "\"reason\":\"Ambiguous\",\"reason\":", 1);
        assert_ne!(ambiguous, raw);
        std::fs::write(input.path(), &ambiguous).unwrap();
        let before = work_snapshot(&root, &work.plan.id);
        let opts = GlobalOptions {
            config: Some(root.join("config.yaml")),
            template_dir: None,
        };
        let command = || WorkCommand::Finish {
            work_id: work.plan.id.clone(),
            file: input.path().to_owned(),
            prepared: None,
        };
        assert!(run(&opts, command()).await.is_err());
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
        assert_eq!(std::fs::read_to_string(input.path()).unwrap(), ambiguous);
        std::fs::write(input.path(), serde_json::to_string_pretty(&result).unwrap()).unwrap();
        run(&opts, command()).await.unwrap();
        let final_path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("final.json");
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(final_path).unwrap()).unwrap(),
            result
        );
    }

    #[test]
    fn ambiguous_final_and_staged_results_refuse_reuse_without_mutating_paid_work() {
        for artifact in ["final.json", "assessment.json"] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let (work, prepared, result) = completed_audit_work(&root);
            let session = MutationSession::acquire(&root).unwrap();
            finish(&session, &work, &prepared, result.clone()).unwrap();
            let path = root
                .join(".lorekeeper/work")
                .join(&work.plan.id)
                .join(artifact);
            let raw = std::fs::read_to_string(&path).unwrap();
            let ambiguous = raw.replacen("\"reason\":", "\"reason\":\"Ambiguous\",\"reason\":", 1);
            assert_ne!(ambiguous, raw);
            std::fs::write(&path, &ambiguous).unwrap();
            let before = work_snapshot(&root, &work.plan.id);
            if artifact == "final.json" {
                assert!(verify_finished_prepared(&root, &work, &prepared).is_err());
                let (config, revision) =
                    lk_core::config::Config::load_with_revision(&root.join("config.yaml")).unwrap();
                assert!(is_finished_prepared(&root, &config, &revision, &work, &prepared).is_err());
            }
            let (config, revision) =
                lk_core::config::Config::load_with_revision(&root.join("config.yaml")).unwrap();
            assert!(
                restage_finished(
                    &session,
                    &config,
                    &revision,
                    &work.plan.id,
                    &lk_vault::ReadControl::default()
                )
                .is_err()
            );
            assert_eq!(work_snapshot(&root, &work.plan.id), before);
            std::fs::write(&path, serde_json::to_string_pretty(&result).unwrap()).unwrap();
            verify_finished_prepared(&root, &work, &prepared).unwrap();
            let before = work_snapshot(&root, &work.plan.id);
            restage_finished(
                &session,
                &config,
                &revision,
                &work.plan.id,
                &lk_vault::ReadControl::default(),
            )
            .unwrap();
            assert_eq!(work_snapshot(&root, &work.plan.id), before);
        }
    }

    #[test]
    fn ambiguous_manifest_context_is_not_adopted_as_current_work() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, _, _) = completed_audit_work(&root);
        let path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("plan.json");
        let raw = std::fs::read_to_string(&path).unwrap();
        let ambiguous = raw.replacen("\"subject\":", "\"subject\":null,\"subject\":", 1);
        assert_ne!(ambiguous, raw);
        std::fs::write(&path, &ambiguous).unwrap();
        let before = work_snapshot(&root, &work.plan.id);
        assert!(Work::load(&root, &work.plan.id).is_err());
        assert!(work.is_planned(&root).is_err());
        assert!(work.clone().with_saved_policy(&root).is_err());
        assert_eq!(work_snapshot(&root, &work.plan.id), before);
    }

    #[cfg(unix)]
    #[test]
    fn prepared_finalizer_inputs_reject_ambiguity_without_changing_byte_pins() {
        let input = tempfile::NamedTempFile::new().unwrap();
        let raw = br#"{"kind":"first","kind":"last"}"#;
        std::fs::write(input.path(), raw).unwrap();
        let revision = lk_core::rss_original::digest(raw);
        for inherited in [None, Some((input.path(), revision.as_str()))] {
            assert!(load_native_finalization_input(Some(input.path()), inherited).is_err());
        }
        assert_eq!(std::fs::read(input.path()).unwrap(), raw);
        let value = json!({"kind":"finalize-section","unknown":null});
        let raw = serde_json::to_vec_pretty(&value).unwrap();
        std::fs::write(input.path(), &raw).unwrap();
        let revision = lk_core::rss_original::digest(&raw);
        assert_eq!(
            load_native_finalization_input(Some(input.path()), None).unwrap(),
            Some(value.clone())
        );
        assert_eq!(
            load_native_finalization_input(None, Some((input.path(), &revision))).unwrap(),
            Some(value)
        );
        assert!(load_native_finalization_input(None, Some((input.path(), "changed"))).is_err());
        assert_eq!(std::fs::read(input.path()).unwrap(), raw);
    }

    #[cfg(unix)]
    #[test]
    fn final_result_fifo_is_refused_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        {
            let session = MutationSession::acquire(&root).unwrap();
            finish(&session, &work, &prepared, result).unwrap();
        }
        let path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("final.json");
        std::fs::remove_file(&path).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(verify_finished_prepared(&root, &work, &prepared).is_err())
                .unwrap();
        });
        assert!(
            receive
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("final result validation waited on a nonregular file")
        );
        worker.join().unwrap();
        use std::os::unix::fs::FileTypeExt;
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_result_symlinks_are_refused_before_following_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let session = MutationSession::acquire(&root).unwrap();
        finish(&session, &work, &prepared, result).unwrap();
        let path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("final.json");
        let raw = std::fs::read(&path).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), &raw).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        assert!(verify_finished_prepared(&root, &work, &prepared).is_err());
        let (config, revision) =
            lk_core::config::Config::load_with_revision(&root.join("config.yaml")).unwrap();
        assert!(is_finished_prepared(&root, &config, &revision, &work, &prepared).is_err());
        assert_eq!(std::fs::read(outside.path()).unwrap(), raw);
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn finished_status_keeps_fresh_scope_and_exact_sealed_result_checks() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, result) = completed_audit_work(&root);
        let (config, config_revision) =
            lk_core::config::Config::load_with_revision(&root.join("config.yaml")).unwrap();
        assert!(!is_finished(&root, &config, &config_revision, &work.plan.id).unwrap());
        #[cfg(unix)]
        assert!(matches!(
            prepare_native_step(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &config_revision,
                &work.plan.id,
                &lk_vault::ReadControl::default()
            )
            .unwrap(),
            NativeStep::Finalization {
                checkpoint: FinalizationCheckpoint {
                    state: NativeFinalization::Ready(FinalizationPreparation::Result(_)),
                    ..
                },
                ..
            }
        ));
        assert!(verify_finished(&root, &config, &config_revision, &work.plan.id).is_err());
        {
            let session = MutationSession::acquire(&root).unwrap();
            finish(&session, &work, &prepared, result).unwrap();
        }
        assert!(is_finished(&root, &config, &config_revision, &work.plan.id).unwrap());
        #[cfg(unix)]
        assert!(matches!(
            prepare_native_step(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &config_revision,
                &work.plan.id,
                &lk_vault::ReadControl::default()
            )
            .unwrap(),
            NativeStep::Finalization {
                checkpoint: FinalizationCheckpoint {
                    state: NativeFinalization::Finished,
                    ..
                },
                ..
            }
        ));
        verify_finished(&root, &config, &config_revision, &work.plan.id).unwrap();

        let final_path = root
            .join(".lorekeeper/work")
            .join(&work.plan.id)
            .join("final.json");
        let original = std::fs::read_to_string(&final_path).unwrap();
        let mut changed: Value = serde_json::from_str(&original).unwrap();
        changed["assessments"][0]["reason"] = json!("A different unsealed assessment.");
        std::fs::write(&final_path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(is_finished(&root, &config, &config_revision, &work.plan.id).is_err());
        #[cfg(unix)]
        assert!(
            prepare_native_step(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &config_revision,
                &work.plan.id,
                &lk_vault::ReadControl::default()
            )
            .is_err()
        );
        assert!(verify_finished(&root, &config, &config_revision, &work.plan.id).is_err());
        std::fs::write(&final_path, &original).unwrap();

        let Subject::Audit { path, .. } = &prepared.subject else {
            unreachable!()
        };
        let subject_path = root.join(path);
        let subject = std::fs::read_to_string(&subject_path).unwrap();
        std::fs::write(
            &subject_path,
            format!("{subject}\nChanged authored evidence.\n"),
        )
        .unwrap();
        assert!(is_finished(&root, &config, &config_revision, &work.plan.id).is_err());
        assert!(verify_finished(&root, &config, &config_revision, &work.plan.id).is_err());
        #[cfg(unix)]
        assert!(
            prepare_native_step(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &config_revision,
                &work.plan.id,
                &lk_vault::ReadControl::default()
            )
            .is_err()
        );
        std::fs::write(subject_path, subject).unwrap();
        assert!(is_finished(&root, &config, &config_revision, &work.plan.id).unwrap());
        assert_eq!(std::fs::read_to_string(final_path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_native_capture_preserves_paid_work_and_does_not_stage_results() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, _, _) = completed_audit_work(&root);
        let (config, revision) =
            lk_core::config::Config::load_with_revision(&root.join("config.yaml")).unwrap();
        let work_dir = root.join(".lorekeeper/work").join(&work.plan.id);
        let snapshot = || {
            walkdir::WalkDir::new(&work_dir)
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| {
                    (
                        entry.path().strip_prefix(&work_dir).unwrap().to_path_buf(),
                        std::fs::read(entry.path()).unwrap(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = snapshot();
        let read = lk_vault::ReadControl::new(std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        ));
        assert!(
            prepare_native_finalization(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &revision,
                &work.plan.id,
                false,
                &read
            )
            .is_err()
        );
        assert!(
            prepare_native_step(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &revision,
                &work.plan.id,
                &read
            )
            .is_err()
        );
        assert!(
            prepare_inspections_batch(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &revision,
                &[(work.plan.id.clone(), 2)],
                &read
            )
            .is_err()
        );
        let session = lk_vault::MutationSession::acquire(&root).unwrap();
        assert!(restage_finished(&session, &config, &revision, &work.plan.id, &read).is_err());
        assert_eq!(snapshot(), before);
        assert!(!work_dir.join("final.json").exists());
        assert!(!work_dir.join("seal.json").exists());
    }

    #[test]
    fn recorded_progress_json_matches_fresh_status_after_reduction_invalidation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (work, prepared, _) = completed_audit_work(&root);
        let node = &work.plan.parts[0].id;
        let input = work.input(&root, node, &prepared.resources).unwrap();
        let session = MutationSession::acquire(&root).unwrap();
        let status = work
            .record(
                &session,
                WorkOutput {
                    work_id: work.plan.id.clone(),
                    node_id: node.clone(),
                    input_revision: input["input_revision"].as_str().unwrap().into(),
                    summary: "A corrected observation from the exact assigned source.".into(),
                    findings: vec![],
                    source_questions: vec![],
                    unresolved: vec![],
                    resolutions: vec![],
                },
                &prepared.resources,
            )
            .unwrap();
        assert!(!status.complete);
        assert!(status.completed < status.total);
        assert!(!status.ready.is_empty());
        #[cfg(unix)]
        {
            let (config, revision) =
                lk_core::config::Config::load_with_revision(&root.join("config.yaml")).unwrap();
            let NativeStep::Inspection {
                progress, request, ..
            } = prepare_native_step(
                &session,
                &config,
                &revision,
                &work.plan.id,
                &lk_vault::ReadControl::default(),
            )
            .unwrap()
            else {
                panic!("corrected incomplete work must not enter finalization");
            };
            assert_eq!(progress.completed, status.completed);
            assert_eq!(progress.ready, status.ready);
            assert_eq!(
                request.unwrap(),
                work.input(&root, &status.ready[0], &prepared.resources)
                    .unwrap()
            );
        }
        let reported = progress_value(&work, &prepared, status, 1, 0);
        assert_eq!(reported, progress(&work, &root, &prepared, 1, 0).unwrap());
        let ready = reported["ready"][0].as_str().unwrap();
        assert_eq!(reported["ready_workload"].as_object().unwrap().len(), 1);
        assert_eq!(reported["ready_workload"][ready]["kind"], "reduce");
        assert_eq!(reported["max_input_bytes"], work.max_input_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn inherited_native_finalization_rejects_corrected_roots_even_without_prepared_flag() {
        let directory = tempfile::tempdir().unwrap();
        let config_path = directory.path().join("config.yaml");
        std::fs::write(&config_path, "vault:\n  root: vault\n  locale: en\nidentity: {name: Tester, email: tester@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n    concept_policy: supported-new\nexecution:\n  work: {extraction_context: full-context}\n").unwrap();
        let (config, config_revision) =
            lk_core::config::Config::load_with_revision(&config_path).unwrap();
        std::fs::create_dir(directory.path().join("vault")).unwrap();
        let root = directory.path().join("vault").canonicalize().unwrap();
        let target = "wiki/documents/subject.md";
        let task = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue")).build_task(
            "ext",
            lk_queue::TaskKind::ExtractConcepts,
            lk_queue::ExtractConceptsRequest {
                concept_policy: lk_core::config::ConceptPolicy::SupportedNew,
                rss_original: None,
                text: "A complete source-scoped observation.".into(),
                source_id: "notes".into(),
                source_type: lk_core::config::SourceType::Manual,
                date: jiff::civil::date(2026, 10, 24),
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
            "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nA complete source-scoped observation.\n\n## Related Concepts\n",
            serde_json::to_string(&task.cache_hash).unwrap()
        );
        write(&root, target, &target_raw);
        write(
            &root,
            ".lorekeeper/queue/fixture.jsonl",
            &serde_json::to_string(&task).unwrap(),
        );
        let session = MutationSession::acquire(&root).unwrap();
        let mut prepared = resources::task(&root, &config, &task.task_id).unwrap();
        let work = persist_prepared(&session, &config, &config_revision, &mut prepared).unwrap();
        assert_eq!(work.plan.parts.len(), 1);
        let (progress, input) = work.prepare_inspection(&root, &prepared.resources).unwrap();
        let input = input.unwrap();
        let mut observation = WorkOutput {
            work_id: work.plan.id.clone(),
            node_id: progress.root_node,
            input_revision: input["input_revision"].as_str().unwrap().into(),
            summary: "The original source observation.".into(),
            findings: vec![],
            source_questions: vec![],
            unresolved: vec![],
            resolutions: vec![],
        };
        assert!(
            work.record(&session, observation.clone(), &prepared.resources)
                .unwrap()
                .complete
        );
        let completed = work.result(&root, &prepared.resources).unwrap();
        let request = native::finalization_request(
            &config,
            &task,
            &work,
            &generation::Owners::build(&config, &prepared).unwrap(),
            None,
            &work.plan.subject_revision,
            &completed,
        )
        .unwrap();
        validate_native_finalization(&root, &config, &work, &prepared, &request).unwrap();
        let bytes = serde_json::to_vec(&request).unwrap();
        let revision = lk_core::rss_original::digest(&bytes);
        let request_path = directory.path().join("prepared.json");
        std::fs::write(&request_path, &bytes).unwrap();
        let inherited = Some((request_path.as_path(), revision.as_str()));
        let captured = load_native_finalization_input(None, inherited)
            .unwrap()
            .unwrap();
        assert_eq!(captured, request);
        assert_eq!(
            load_native_finalization_input(Some(&request_path), inherited).unwrap(),
            Some(request.clone())
        );
        let other_path = directory.path().join("different.json");
        std::fs::write(&other_path, b"{}").unwrap();
        assert!(load_native_finalization_input(Some(&other_path), inherited).is_err());
        std::fs::write(&request_path, b"{}").unwrap();
        assert!(load_native_finalization_input(None, inherited).is_err());
        std::fs::write(&request_path, bytes).unwrap();

        observation.summary = "A corrected observation of the same assigned source.".into();
        assert!(
            work.record(&session, observation, &prepared.resources)
                .unwrap()
                .complete
        );
        let (current, prepared) = load(&root, &config, &config_revision, &work.plan.id).unwrap();
        assert_eq!(current, work);
        assert_ne!(
            current.result(&root, &prepared.resources).unwrap(),
            completed
        );
        let Subject::Queue {
            target_revision, ..
        } = &prepared.subject
        else {
            unreachable!()
        };
        let result = json!({
            "task_id":task.task_id, "cache_hash":task.cache_hash,
            "target":task.target, "target_revision":target_revision,
            "context_revision":work.plan.subject_revision,
            "concept_policy":"supported-new", "date":"2026-10-24",
            "discovery":[], "concepts":[],
        });
        let request = load_native_finalization_input(None, inherited)
            .unwrap()
            .unwrap();
        assert!(
            finish_with_preparation(
                &session,
                &config,
                &current,
                &prepared,
                result.clone(),
                Some(&request),
            )
            .is_err()
        );
        let work_dir = PathBuf::from(".lorekeeper/work").join(&work.plan.id);
        for path in [
            work_dir.join("seal.json"),
            work_dir.join("final.json"),
            queue_result_path(task.kind, &result).unwrap(),
        ] {
            assert!(!root.join(path).exists());
        }
        assert_eq!(
            std::fs::read_to_string(root.join(target)).unwrap(),
            target_raw
        );
        let request = load_native_finalization_input(None, None).unwrap();
        assert!(request.is_none());
        finish_with_preparation(
            &session,
            &config,
            &current,
            &prepared,
            result,
            request.as_ref(),
        )
        .unwrap();
        verify_finished(&root, &config, &config_revision, &work.plan.id).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join(target)).unwrap(),
            target_raw
        );
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

    #[cfg(unix)]
    #[test]
    fn mixed_recording_refuses_changed_configuration_and_root_at_role_boundaries() {
        for (drift, phase) in [
            (None, 0),
            (Some("config"), 1),
            (Some("config"), 2),
            (Some("root"), 1),
            (Some("root"), 2),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let root = base.join("vault");
            let alternate = base.join("alternate");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&alternate).unwrap();
            let selector = base.join("selected-vault");
            std::os::unix::fs::symlink(&root, &selector).unwrap();
            let config_path = base.join("config.yaml");
            std::fs::write(&config_path, "vault:\n  root: selected-vault\n  locale: en\nidentity: {name: Tester, email: tester@example.com}\nsources:\n  notes:\n    type: manual\n    params: {inbox_dir: inbox}\n    concept_policy: supported-new\nexecution:\n  default_profile: reader\n  profiles:\n    reader:\n      engine: codex\n      timeout_seconds: 30\n      work: {extraction_context: progressive, max_input_bytes: 16384, max_result_bytes: 4096}\n").unwrap();
            let (mut config, revision) =
                lk_core::config::Config::load_with_revision(&config_path).unwrap();
            config.execution.select_profile("reader").unwrap();
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
                        vault_path: "wiki/documents/source.md".into(),
                        kind: lk_queue::TargetKind::DocumentConcepts,
                        anchor: "## Related Concepts".into(),
                    },
                },
            );
            let raw = format!(
                "---\ntype: document\nllm_inputs:\n  concepts: {}\n---\n## Content\nCandidate source evidence.\n\n## Related Concepts\n",
                task.cache_hash
            );
            write(&root, &task.target.vault_path, &raw);
            write(
                &root,
                "wiki/concepts/candidate.md",
                "---\ntype: concept\ntitle: Candidate\n---\n## Synthesis\nCandidate evidence.\n",
            );
            write(
                &root,
                "wiki/documents/audit.md",
                "---\ntype: document\ntitle: Independent\n---\n## Content\nIndependent audit observation.\n",
            );
            write(
                &root,
                ".lorekeeper/queue/fixture.jsonl",
                &serde_json::to_string(&task).unwrap(),
            );
            let session = MutationSession::acquire(&root).unwrap();
            discovery::record(
                &session,
                &task.task_id,
                discovery::DiscoveryRequest {
                    target_revision: file_revision(&raw),
                    queries: vec![lk_core::discovery::DiscoveryQuery {
                        query: "Candidate".into(),
                        phrase: true,
                        spacing: false,
                    }],
                },
            )
            .unwrap();
            let screening = || {
                current_screening(&root, &config, &revision, &task.task_id)
                    .unwrap()
                    .into_candidates()
                    .unwrap()
            };
            let request =
                serde_json::to_value(screening().selection.next_request(false).unwrap().unwrap())
                    .unwrap();
            assert!(
                !root.join(".lorekeeper/queue/screening").exists(),
                "issuing a request writes nothing"
            );
            let decisions = request["candidates"].as_array().unwrap().iter().map(|card| json!({
                "candidate_key":card["candidate_key"],"decision":"not-selected","reason":"Card-scoped exclusion."
            })).collect::<Vec<_>>();
            let screen = ScreeningResponse::Decisions {
                output: json!({
                    "task_id":request["task_id"],"request_revision":request["request_revision"],
                    "decisions":decisions,
                }),
                request,
            };
            let prepared = resources::audit(&root, &config, "wiki/documents/audit.md").unwrap();
            let work = Work::create(
                &session,
                &prepared.resources,
                &prepared.revision(&revision).unwrap(),
                prepared.context.clone(),
                limits(&config).unwrap(),
            )
            .unwrap();
            let input = work
                .prepare_inspections(&root, &prepared.resources, 1)
                .unwrap()
                .1
                .remove(0);
            let reply = InspectionReply {
                work_id: work.plan.id.clone(),
                node_id: input["node_id"].as_str().unwrap().into(),
                request: input,
                response: InspectionResponse::NeedsAction(
                    "Independent evidence requires reassessment.".into(),
                ),
            };
            let before = work_snapshot(&root, &work.plan.id);
            let pin =
                super::super::ConfigurationPin::from_loaded(config_path.clone(), revision.clone());
            let root_pin = super::super::workflow::VaultRootPin::capture(&selector, &root).unwrap();
            let checks = std::cell::Cell::new(0);
            let records = record_read_wave(
                &session,
                &config,
                &revision,
                vec![(&task.task_id, screen)],
                vec![reply],
                || {
                    checks.set(checks.get() + 1);
                    if checks.get() == phase {
                        match drift {
                            Some("config") => {
                                let raw = std::fs::read_to_string(&config_path).unwrap();
                                std::fs::write(&config_path, raw + "# changed selection\n")
                                    .unwrap();
                            }
                            Some("root") => {
                                std::fs::remove_file(&selector).unwrap();
                                std::os::unix::fs::symlink(&alternate, &selector).unwrap();
                            }
                            _ => unreachable!(),
                        }
                    }
                    pin.validate()?;
                    root_pin.validate()
                },
                &lk_vault::ReadControl::default(),
            );
            if phase == 1 {
                assert!(records.screenings.is_none());
            } else {
                assert!(matches!(
                    records.screenings.unwrap()[0].result,
                    Ok(ScreeningRecordOutcome::Recorded(1))
                ));
            }
            if drift.is_some() {
                assert!(matches!(
                    records.inspections,
                    InspectionRecording::Invalidated(_)
                ));
                assert_eq!(work_snapshot(&root, &work.plan.id), before);
            } else {
                let InspectionRecording::Recorded(records) = records.inspections else {
                    panic!("current evidence must record");
                };
                assert!(matches!(
                    records.unwrap().records[0].result,
                    Ok(InspectionRecordOutcome::Deferred(_))
                ));
                assert_ne!(work_snapshot(&root, &work.plan.id), before);
            }
            assert_eq!(screening().selection.progress().complete, phase != 1);
            assert!(std::fs::read_dir(&alternate).unwrap().next().is_none());
            assert!(!root.join(".lorekeeper/mutation/pending.json").exists());
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
        let turn = prepare_screenings(
            &MutationSession::acquire(&root).unwrap(),
            &config,
            &config_revision,
            &[&task.task_id],
            &lk_vault::ReadControl::default(),
        )
        .unwrap()
        .turns
        .remove(0);
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let read = lk_vault::ReadControl::new(cancelled.clone());
        let (attempts, _, _) = map_current_screenings(
            &root,
            &config,
            &config_revision,
            &[&task.task_id, &task.task_id],
            |screening, _| {
                let accepted = screening.is_ok();
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                accepted
            },
            &read,
        )
        .unwrap();
        assert_eq!(attempts, [true, false]);
        assert_eq!((turn.decided, turn.candidates), (0, 2));
        let ScreeningState::Ready(request) = turn.state else {
            panic!("unrecorded screening must be ready");
        };
        assert!(
            !root.join(".lorekeeper/queue/screening").exists(),
            "preparing a request writes nothing"
        );
        let decisions = request["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|candidate| {
                json!({"candidate_key":candidate["candidate_key"],
                "decision":if candidate["owner"] == selected {"inspect"} else {"not-selected"},
                "reason":"Decide this source-scoped card before reading selected evidence."})
            })
            .collect::<Vec<_>>();
        MutationSession::acquire(&root)
            .unwrap()
            .apply(
                selection::plan_record(
                    &root,
                    &selection::Request::from_value(&request).unwrap(),
                    serde_json::from_value(json!({
                        "task_id":request["task_id"],
                        "request_revision":request["request_revision"],
                        "decisions":decisions,
                    }))
                    .unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let context = || {
            task_context(
                &root,
                &config,
                &config_revision,
                &task.task_id,
                Some(&queries),
                None,
            )
            .unwrap()
        };
        assert_eq!(
            context()["status"],
            "needs-screening",
            "a decided roster is not ready until it is sealed"
        );
        let binding = seal_roster(
            &root,
            &super::super::ConfigurationPin::Absent,
            &current_screening(&root, &config, &config_revision, &task.task_id)
                .unwrap()
                .into_candidates()
                .unwrap()
                .selection,
        )
        .unwrap();
        let ready = context();
        assert_eq!(ready["status"], "ready");
        assert_eq!(ready["selection"], serde_json::to_value(&binding).unwrap());
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
        assert!(
            !prepared.pages.contains_key(Path::new(excluded)),
            "a candidate that was not selected is bound by its judged card"
        );
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
            let prepared = {
                let session = MutationSession::acquire(&root).unwrap();
                prepare_task(&session, &config, &config_revision, &task.task_id)
                    .unwrap()
                    .work
            };
            let TaskWork::Partitioned { id } = prepared else {
                panic!("fixture must require bounded work");
            };
            let (work, prepared) = load_controlled(
                &root,
                &config,
                &config_revision,
                &id,
                &lk_vault::ReadControl::default(),
            )
            .unwrap();
            let (before, input) = work.prepare_inspection(&root, &prepared.resources).unwrap();
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
            #[cfg(unix)]
            let captured = (!added_candidate).then(|| {
                load_inspection_works(
                    &root,
                    &config,
                    &config_revision,
                    &[&id],
                    &lk_vault::ReadControl::default(),
                )
                .unwrap()
            });
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
            #[cfg(unix)]
            if let Some(captured) = captured {
                let recorded = record_prepared_inspections(
                    &MutationSession::acquire(&root).unwrap(),
                    captured,
                    vec![InspectionReply {
                        work_id: id.clone(),
                        node_id: node.clone(),
                        request: input.clone(),
                        response: InspectionResponse::Complete(Box::new(
                            state::ObservationCandidate {
                                output: output.clone(),
                                text_violation: None,
                                response_revision: lk_core::rss_original::digest(b"exact response"),
                                response_json_bytes: 300,
                            },
                        )),
                    }],
                    &lk_vault::ReadControl::default(),
                )
                .unwrap();
                assert!(recorded.records[0].result.is_err());
            }
            assert!(
                record_inspection(
                    &MutationSession::acquire(&root).unwrap(),
                    &config,
                    &config_revision,
                    &id,
                    node,
                    output.clone(),
                    &lk_vault::ReadControl::default(),
                )
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
            let after = record_inspection(
                &MutationSession::acquire(&root).unwrap(),
                &config,
                &config_revision,
                &id,
                node,
                output,
                &lk_vault::ReadControl::default(),
            )
            .unwrap();
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
