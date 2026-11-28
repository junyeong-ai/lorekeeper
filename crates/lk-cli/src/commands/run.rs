use clap::ValueEnum;
use serde::Serialize;

#[cfg(unix)]
pub(super) mod automation;
#[cfg(unix)]
mod scheduler;

#[derive(clap::Args)]
pub struct RunOptions {
    /// Complete daily collection, queued processing, or knowledge audit batches
    #[arg(value_enum)]
    operation: Operation,
    /// Job profile and work budget; otherwise operation override then default. Stage overrides select hosts only.
    #[arg(long)]
    profile: Option<String>,
    /// Maximum process or knowledge batches; release the workflow lease between batches
    #[arg(long, default_value = "1")]
    batches: std::num::NonZeroUsize,
    /// Wait for the semantic workflow lease; default contention starts no semantic work
    #[arg(long)]
    wait: bool,
    /// Emit the structured job outcome; stage output goes to stderr
    #[arg(long)]
    json: bool,
    /// Continue ready work within the configured automation duration and shared UTC-day allowance
    #[arg(long, conflicts_with = "batches")]
    scheduled: bool,
    /// Discard host transcripts: report Lore stages and bounded host diagnostics only (implied by --scheduled)
    #[arg(long)]
    unattended: bool,
    /// Explicitly resume this paused scheduled operation without resetting its allowance
    #[arg(long, requires = "scheduled")]
    resume: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    Daily,
    Process,
    Knowledge,
}

impl Operation {
    #[cfg(unix)]
    fn semantic(self) -> lk_core::execution::ExecutionOperation {
        match self {
            Self::Daily => lk_core::execution::ExecutionOperation::Daily,
            Self::Process => lk_core::execution::ExecutionOperation::Process,
            Self::Knowledge => lk_core::execution::ExecutionOperation::Knowledge,
        }
    }
}

pub async fn run(
    opts: &super::GlobalOptions,
    installation: &lk_dist::Installation,
    options: RunOptions,
) -> miette::Result<()> {
    #[cfg(unix)]
    {
        native::run(opts, installation, options).await
    }
    #[cfg(not(unix))]
    {
        let RunOptions {
            operation,
            profile,
            batches,
            wait,
            json,
            scheduled,
            unattended,
            resume,
        } = options;
        let _ = (
            opts,
            installation,
            operation,
            profile,
            batches,
            wait,
            json,
            scheduled,
            unattended,
            resume,
        );
        Err(miette::miette!(
            "agent jobs require Unix workflow leases; Windows is unsupported"
        ))
    }
}

#[cfg(unix)]
mod native {
    mod audit;
    mod preparation;
    mod replans;
    use replans::{AuditDeferrals, AuditReplans, FinalizationReplans};

    use super::*;
    use super::{automation, scheduler};
    use crate::commands::audit::{DeferredAudit, InputAvailability};
    use crate::commands::queue::application::{is_semantic_context_changed, is_work_input_changed};
    use crate::commands::{
        CONFIG_REVISION_ENV, EXECUTION_PROFILE_ENV, GlobalOptions, execution, typed_error, workflow,
    };
    use lk_core::config::Config;
    use lk_core::execution::{ExecutionProfile, ExecutionStage};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum WorkRole {
        Discovery,
        Evidence,
        Screening,
        Finalizer,
    }

    #[derive(Clone, Copy, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum NodeKind {
        Inspect,
        Reduce,
    }

    #[derive(Clone, Copy, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum WorkOutcome {
        /// The step ended before deciding an outcome: a stage record names the failure or
        /// replanning that ended it. Every decided result sets its own outcome.
        Unsettled,
        Recorded,
        Refined,
        Staged,
        Restaged,
        AlreadyComplete,
        /// The reply was unusable for its request and was not adopted.
        Rejected,
        /// The request was prepared but never sent: its host was unavailable, the run's calls
        /// were spent, or a halt or writer wait discarded its capture, as a stage record names.
        /// Its work is unchanged and still pending.
        Unsent,
        /// The step's inputs changed before its reply or preparation could be used, and the
        /// reply was discarded: the work is replanned against current inputs, in the next run
        /// when the run's own configuration changed, which stops it.
        InputsChanged,
        /// The reply or its preparation stated a needed operator action, retained as a receipt.
        NeedsAction,
        /// The synthesis carried a review proposal this run could not publish, its target's one
        /// review per run being spent, so the target was deferred. Nothing of the reply is kept:
        /// a later run drafts the synthesis, and its proposal, again.
        ReviewDeferred,
        Failed,
        Cancelled,
        Yielded,
    }

    #[derive(Serialize)]
    struct WorkStepMetrics {
        role: WorkRole,
        stages: Vec<ExecutionStage>,
        execution: Option<RequestedExecution>,
        host_failure: Option<execution::diagnostic::HostFailure>,
        #[serde(skip)]
        channel: HostChannel,
        /// The host ended without an answer (a timeout, a failed exit, empty output or a missing
        /// terminal result), as distinct from answering with an unusable one.
        #[serde(skip)]
        unanswered: bool,
        /// The host configuration that ended this step's invocation, and when.
        #[serde(skip)]
        host_end: Option<crate::commands::work::state::HostEnd>,
        /// The host returned a response, usable or not: only such a step shows that the
        /// engine serves requests. A failure to launch or an answerless exit never does.
        #[serde(skip)]
        answered: bool,
        work_id: Option<String>,
        node_id: Option<String>,
        node_kind: Option<NodeKind>,
        workload: Option<crate::commands::work::workload::ObservationWorkload>,
        screening_id: Option<String>,
        request_revision: Option<String>,
        // Compact UTF-8 JSON, excluding prompts and host envelopes. Results measure
        // the decoded value before recording.
        request_json_bytes: Option<usize>,
        result_json_bytes: Option<usize>,
        preparation_duration_ms: Option<u64>,
        preparation_from_queue_plan: bool,
        agent_duration_ms: Option<u64>,
        revalidation_record_duration_ms: Option<u64>,
        verification_duration_ms: Option<u64>,
        outcome: WorkOutcome,
    }

    impl WorkStepMetrics {
        fn new(role: WorkRole) -> Self {
            Self {
                role,
                stages: match role {
                    WorkRole::Screening => vec![ExecutionStage::Screening],
                    _ => Vec::new(),
                },
                execution: None,
                host_failure: None,
                channel: HostChannel::Observation,
                unanswered: false,
                host_end: None,
                answered: false,
                work_id: None,
                node_id: None,
                node_kind: None,
                workload: None,
                screening_id: None,
                request_revision: None,
                request_json_bytes: None,
                result_json_bytes: None,
                preparation_duration_ms: None,
                preparation_from_queue_plan: false,
                agent_duration_ms: None,
                revalidation_record_duration_ms: None,
                verification_duration_ms: None,
                outcome: WorkOutcome::Unsettled,
            }
        }
    }

    #[derive(Default, Serialize)]
    struct RetainedProgress {
        evidence_nodes: usize,
        screening_packets: usize,
        staged_requests: usize,
        discovery_recipes: usize,
        /// Open reviews a synthesis finalizer proposed and the run published for its concept.
        synthesis_reviews: usize,
        action_receipts: usize,
        queue_publication_observed: bool,
    }

    impl RetainedProgress {
        fn has_progress(&self) -> bool {
            self.evidence_nodes != 0
                || self.screening_packets != 0
                || self.staged_requests != 0
                || self.discovery_recipes != 0
                || self.synthesis_reviews != 0
                || self.action_receipts != 0
                || self.queue_publication_observed
        }
    }

    #[derive(Default, Serialize)]
    struct WorkTelemetry {
        retained_progress: RetainedProgress,
        work_steps: Vec<WorkStepMetrics>,
        work_steps_omitted: usize,
        finalization_admissions: Vec<crate::commands::work::native::FinalizationAdmission>,
        finalization_admissions_omitted: usize,
        evidence_preparation_duration_ms: u64,
        evidence_revalidation_duration_ms: u64,
        evidence_record_duration_ms: u64,
        screening_preparation_io: lk_vault::DiscoveryCaptureMetrics,
        screening_record_duration_ms: u64,
    }

    impl WorkTelemetry {
        const MAX_STEPS: usize = 1024;

        fn record_admission(
            &mut self,
            admission: crate::commands::work::native::FinalizationAdmission,
        ) {
            if self.finalization_admissions.len() < Self::MAX_STEPS {
                self.finalization_admissions.push(admission);
            } else {
                self.finalization_admissions_omitted += 1;
            }
        }

        fn record(&mut self, step: WorkStepMetrics) {
            match (step.role, step.outcome) {
                (WorkRole::Evidence, WorkOutcome::Recorded) => {
                    self.retained_progress.evidence_nodes += 1
                }
                (WorkRole::Screening, WorkOutcome::Recorded) => {
                    self.retained_progress.screening_packets += 1
                }
                (WorkRole::Finalizer, WorkOutcome::Staged) => {
                    self.retained_progress.staged_requests += 1
                }
                _ => {}
            }
            if self.work_steps.len() < Self::MAX_STEPS {
                eprintln!(
                    "{}",
                    serde_json::json!({"type":"lore.work-step", "step":step})
                );
                self.work_steps.push(step);
            } else {
                self.work_steps_omitted += 1;
            }
        }
    }

    enum ObservationReply {
        /// A host that exited and answered on stdout; its diagnostic keeps the exit and stderr.
        Output {
            engine: lk_core::execution::AgentEngine,
            child: execution::ChildResult,
            limit: usize,
        },
        InvocationFailed(crate::commands::work::state::InvocationFailure),
        /// Not sent: its host tripped `HOST_FAILURE_LIMIT` in this run.
        HostUnavailable(String),
    }

    enum ResultReply {
        Complete(serde_json::Value),
        InvocationFailed(crate::commands::work::state::InvocationFailure),
        /// Not sent: its host tripped `HOST_FAILURE_LIMIT` in this run.
        HostUnavailable(String),
    }

    enum ObservationStep {
        Recorded,
        Refined,
        Deferred(String),
        /// The Work's inputs changed while its host ran; the reply is discarded unrecorded.
        InputsChanged(miette::Report),
        InvocationFailed,
        Failed,
    }

    struct PreparedObservations {
        completed: usize,
        requests: std::collections::VecDeque<serde_json::Value>,
    }

    struct DecodedScreenings {
        accepted: bool,
        replies: Vec<ScreeningReply>,
        metrics: Vec<WorkStepMetrics>,
        /// A turn's host could not start; its profile is deferred for the run.
        unstarted: bool,
    }

    struct DecodedObservations {
        accepted: bool,
        replies: Vec<crate::commands::work::InspectionReply>,
        returned: Vec<ObservationTurn>,
        /// A turn's host could not start; its profile is deferred for the run.
        unstarted: bool,
    }

    struct ObservationTurn {
        /// The queue target this Work answers; audit Work replans through subject allowances.
        target: Option<String>,
        /// The bounded audit subject (its job key) this Work answers, replanned by its route.
        audit_subject: Option<String>,
        work_id: String,
        node_id: String,
        completed: usize,
        request: serde_json::Value,
        metrics: WorkStepMetrics,
    }

    struct ScreeningAttempt {
        task_id: String,
        decided: usize,
        candidates: usize,
        request: serde_json::Value,
        metrics: WorkStepMetrics,
    }

    struct ScreeningReply {
        task_id: String,
        output: crate::commands::work::ScreeningResponse,
        decided: usize,
        candidates: usize,
        metric_index: usize,
    }

    enum QueueReadTurn {
        Evidence(ObservationTurn),
        Screening(ScreeningAttempt),
    }

    impl QueueReadTurn {
        fn role(&self) -> WorkRole {
            match self {
                Self::Evidence(_) => WorkRole::Evidence,
                Self::Screening(_) => WorkRole::Screening,
            }
        }

        fn request(&self) -> &serde_json::Value {
            match self {
                Self::Evidence(turn) => &turn.request,
                Self::Screening(turn) => &turn.request,
            }
        }

        fn metrics_mut(&mut self) -> &mut WorkStepMetrics {
            match self {
                Self::Evidence(turn) => &mut turn.metrics,
                Self::Screening(turn) => &mut turn.metrics,
            }
        }
        fn metrics(&self) -> &WorkStepMetrics {
            match self {
                Self::Evidence(turn) => &turn.metrics,
                Self::Screening(turn) => &turn.metrics,
            }
        }
        fn into_metrics(self) -> WorkStepMetrics {
            match self {
                Self::Evidence(turn) => turn.metrics,
                Self::Screening(turn) => turn.metrics,
            }
        }
    }

    enum WorkStep {
        Pending,
        Staged,
        Deferred(String),
        Replan,
        Failed,
    }

    /// The host a call ran through: tool-free observation, or the structured tool-result host
    /// that bounded finalizers use.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum HostChannel {
        Observation,
        Result,
    }

    /// Pending unanswered holds a run may release, one at a time, as the probe an operator would
    /// send, and the one released now.
    #[derive(Default)]
    struct HostProbe {
        /// The candidates still to try, `None` until the drain first chooses among them.
        remaining: Option<std::collections::VecDeque<crate::commands::work::host::PendingHold>>,
        released: Option<crate::commands::work::host::PendingHold>,
        /// The queue target or audit subject the released hold belongs to, which its drain
        /// sends before other work.
        target: Option<String>,
    }

    /// The queue target or audit subject a released hold belongs to.
    #[cfg(unix)]
    fn probe_hold_target(
        root: &Path,
        hold: &crate::commands::work::host::PendingHold,
    ) -> Option<String> {
        match &hold.scope {
            crate::commands::work::host::HoldScope::Queue { task_id, .. } => {
                crate::commands::queue::current_task(root, task_id)
                    .ok()
                    .map(|task| task.target.vault_path)
            }
            crate::commands::work::host::HoldScope::Audit { path, .. } => Some(path.clone()),
        }
    }

    /// Why a run releases a probe.
    #[derive(Clone, Copy)]
    enum ProbeRelease {
        /// Planning found nothing else admissible.
        Idle,
        /// The previous run of this drain gave its turn to ordinary work while probes waited.
        RecoveryTurn,
        /// The hold released before cannot be planned now: its target is no longer current
        /// work, or it waits on unfinished dependencies.
        Unplannable,
    }

    /// The probe each drain is choosing. A queue drain and a knowledge audit reach different
    /// holds, so neither continues the other's candidates.
    #[derive(Default)]
    struct HostProbes {
        queue: Option<HostProbe>,
        audit: Option<HostProbe>,
    }

    impl HostProbes {
        fn scope(
            &mut self,
            scope: crate::commands::work::host::ProbeScope,
        ) -> &mut Option<HostProbe> {
            match scope {
                crate::commands::work::host::ProbeScope::Queue => &mut self.queue,
                crate::commands::work::host::ProbeScope::Audit => &mut self.audit,
            }
        }

        /// The hosts of the probes released, by engine and the host that failed.
        fn released(
            &self,
        ) -> impl Iterator<
            Item = (
                Option<lk_core::execution::AgentEngine>,
                Option<crate::commands::work::host::ProfileKey>,
            ),
        > {
            [&self.queue, &self.audit]
                .into_iter()
                .flatten()
                .filter_map(|probe| probe.released.as_ref())
                .map(|hold| {
                    (
                        hold.engine,
                        hold.host.as_ref().map(|host| {
                            crate::commands::work::host::ProfileKey::from_profile(&host.profile)
                        }),
                    )
                })
        }
    }

    /// The host configuration a call ran on, by role, host and execution profile: a failure of
    /// one configuration says nothing about another.
    type HostKey = (WorkRole, HostChannel, Option<String>);

    /// A call this run will not send because its host configuration tripped
    /// `HOST_FAILURE_LIMIT`: the caller defers only that work for the rest of the run.
    #[derive(Debug)]
    struct HostUnavailable(String);

    impl std::fmt::Display for HostUnavailable {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(formatter)
        }
    }

    impl std::error::Error for HostUnavailable {}
    impl miette::Diagnostic for HostUnavailable {}

    fn host_unavailable(profile: &str, reason: &str) -> HostUnavailable {
        HostUnavailable(format!(
            "profile {profile}: {reason}; its work is deferred to a later run"
        ))
    }

    /// Consecutive unanswered invocation failures of one host configuration, with no answered call
    /// on it between them, that end a run. One failure is usually its own request's and is held
    /// or replanned while the run continues; a repeat points at a fault shared by every
    /// request, which further calls would only spend.
    const HOST_FAILURE_LIMIT: usize = 2;

    const FINALIZATION_CHANGED_AGAIN: &str =
        "finalizer inputs changed again after fresh replanning; retained for a later run";

    const EVIDENCE_CHANGED_AGAIN: &str =
        "evidence inputs changed again after fresh replanning; retained for a later run";

    /// The reason an audit subject whose inputs changed again after its replan is deferred.
    const AUDIT_UNSTABLE: &str =
        "audit inputs changed again after fresh replanning; retained for a later run";

    #[derive(Default, PartialEq, Eq)]
    enum QueueReplan {
        #[default]
        None,
        Refresh,
        Invalidate,
    }

    impl QueueReplan {
        fn is_requested(&self) -> bool {
            *self != Self::None
        }

        fn refresh(&mut self) {
            if *self == Self::None {
                *self = Self::Refresh;
            }
        }
    }

    enum QueueDispatch {
        Discovery {
            task: lk_queue::QueueTask,
        },
        Partitioned {
            id: String,
            task: lk_queue::QueueTask,
        },
        Finalization {
            id: String,
            task: lk_queue::QueueTask,
        },
        RootFinalization {
            id: String,
            task: lk_queue::QueueTask,
        },
        Source {
            id: String,
        },
        Screening {
            task: lk_queue::QueueTask,
            screening_id: String,
            prepared: Option<Box<crate::commands::work::ScreeningTurn>>,
            prepared_at_call: usize,
        },
        Section {
            task: lk_queue::QueueTask,
        },
        WholeInput {
            tasks: Vec<lk_queue::QueueTask>,
            inputs: Vec<serde_json::Value>,
        },
    }

    impl QueueDispatch {
        fn is_read_only(&self) -> bool {
            matches!(
                self,
                Self::Partitioned { .. } | Self::Source { .. } | Self::Screening { .. }
            )
        }

        fn binding(&self, target: &str) -> Option<scheduler::TurnBinding> {
            let identity = match self {
                Self::Partitioned { id, .. }
                | Self::Source { id }
                | Self::RootFinalization { id, .. } => format!("work:{id}"),
                Self::Finalization { id, .. } => format!("finalize:{id}"),
                Self::Screening { screening_id, .. } => format!("screening:{screening_id}"),
                Self::Discovery { .. } | Self::Section { .. } | Self::WholeInput { .. } => {
                    return None;
                }
            };
            Some(scheduler::TurnBinding {
                target: target.into(),
                identity,
            })
        }
    }

    struct QueuePlan {
        jobs: std::collections::BTreeMap<String, QueueDispatch>,
        deferred: std::collections::BTreeMap<String, Vec<String>>,
        keys: std::collections::BTreeSet<String>,
        needs_application: bool,
    }

    const MAX_PREPARED_QUEUE_TARGETS: usize = 64;

    fn is_whole_input_batch_candidate(
        kind: lk_queue::TaskKind,
        context: lk_core::execution::ExtractionContext,
    ) -> bool {
        match kind {
            lk_queue::TaskKind::SynthesizeConcept => true,
            lk_queue::TaskKind::ExtractConcepts => {
                context == lk_core::execution::ExtractionContext::FullContext
            }
            lk_queue::TaskKind::Summarize
            | lk_queue::TaskKind::RefineEvents
            | lk_queue::TaskKind::IdentifyThemes => false,
        }
    }

    fn generation_stage(kind: lk_queue::TaskKind) -> ExecutionStage {
        match kind {
            lk_queue::TaskKind::Summarize
            | lk_queue::TaskKind::RefineEvents
            | lk_queue::TaskKind::IdentifyThemes => ExecutionStage::Section,
            lk_queue::TaskKind::ExtractConcepts => ExecutionStage::Extraction,
            lk_queue::TaskKind::SynthesizeConcept => ExecutionStage::Synthesis,
        }
    }

    struct QueueTurn {
        keys: std::collections::BTreeSet<String>,
        targets: Vec<String>,
        binding: Option<scheduler::TurnBinding>,
    }

    #[derive(Clone, Copy, Default, Serialize)]
    struct QueuePlanningMetrics {
        attempts: usize,
        duration_ms: u64,
        synthesis_subjects: usize,
        audit_captures: usize,
        prepared_targets: usize,
        discovery_captures: usize,
        discovery_io: lk_vault::DiscoveryCaptureMetrics,
    }

    #[derive(Default, Serialize)]
    struct MutationAccessMetrics {
        attempts: usize,
        duration_ms: u64,
    }

    #[derive(Default, Serialize)]
    struct QueueApplicationMetrics {
        attempts: usize,
        duration_ms: u64,
        applied: usize,
        dropped: usize,
        rejected: usize,
        orphaned: usize,
        superseded: usize,
        held: usize,
        latest: Option<crate::commands::queue::application::Report>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum StageState {
        Completed,
        Skipped,
        Failed,
        Cancelled,
    }

    #[derive(Serialize)]
    struct StageResult {
        name: String,
        state: StageState,
        detail: Option<String>,
        /// When the stage was recorded, on the clock `lore.*` events report as
        /// `elapsed_job_ms`. The gap since the previous record also covers unrecorded work
        /// and waits between them; planning, mutation access and workflow waits keep their
        /// own duration fields.
        elapsed_job_ms: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum YieldReason {
        WorkflowContention,
    }

    #[derive(Serialize)]
    struct ExecutionInfo {
        #[serde(flatten)]
        host: RequestedExecution,
        policy_profile: String,
        work: lk_core::execution::SemanticWorkConfig,
    }

    impl WorkStepMetrics {
        fn host(&self) -> HostKey {
            (
                self.role,
                self.channel,
                self.execution
                    .as_ref()
                    .map(|execution| execution.profile.clone()),
            )
        }
    }

    #[derive(Clone, Serialize)]
    struct RequestedExecution {
        profile: String,
        engine: lk_core::execution::AgentEngine,
        version: String,
        requested_model: Option<String>,
        requested_effort: Option<String>,
    }

    struct PreparedAgent {
        profile: ExecutionProfile,
        execution: RequestedExecution,
    }

    #[derive(Serialize)]
    struct AuditProgress {
        before: lk_graph::audit::AuditCoverage,
        after: Option<lk_graph::audit::AuditCoverage>,
    }

    /// What the failed stage that stopped a job asks of its scheduled operation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum HaltKind {
        /// State the operator repairs (accounting, integrity, configuration, an unknown host
        /// failure): a scheduled operation pauses until `--resume`.
        Fault,
        /// A writer changed or removed inputs while the job read them (`is_input_drift`): the
        /// batch ends with its unfinished work retained, and the next run plans again.
        InputsChanged,
    }

    #[derive(Clone, Debug)]
    struct Halt {
        kind: HaltKind,
        cause: automation::HaltCause,
    }

    /// `halted` and, for a halted job, `halt_kind`.
    #[derive(Default)]
    struct HaltState(Option<Halt>);

    impl Serialize for HaltState {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(None)?;
            map.serialize_entry("halted", &self.0.is_some())?;
            if let Some(halt) = &self.0 {
                map.serialize_entry("halt_kind", &halt.kind)?;
            }
            map.end()
        }
    }

    /// Whether `error` is input drift: a page, citation, task or capture a writer edited or
    /// removed after the job read it, or a publication guard that found guarded bytes changed.
    /// The halt decision reads only these typed errors. Integrity faults (an immutable capture
    /// missing or changed, corrupt state, an unreadable present page), host failures and
    /// configuration or root drift are not drift.
    fn is_input_drift(error: &miette::Report) -> bool {
        use lk_graph::GraphError;
        let vault_drift = |error: &lk_vault::VaultError| {
            matches!(
                error,
                lk_vault::VaultError::MutationConflict(_)
                    | lk_vault::VaultError::RssOriginChanged(_)
            )
        };
        if let Some(rejection) =
            error.downcast_ref::<crate::commands::queue::application::PublicationRejection>()
        {
            return rejection.kind.is_input_change();
        }
        error
            .downcast_ref::<crate::commands::work::SourceChanged>()
            .is_some()
            || error
                .downcast_ref::<crate::commands::queue::TaskNotCurrent>()
                .is_some_and(crate::commands::queue::TaskNotCurrent::is_input_change)
            || error
                .downcast_ref::<lk_vault::VaultError>()
                .is_some_and(vault_drift)
            || error
                .downcast_ref::<GraphError>()
                .is_some_and(|error| match error {
                    GraphError::AuditInputChanged(_)
                    | GraphError::AuditScreeningChanged
                    | GraphError::AuditSubjectRemoved(_)
                    | GraphError::AuditSubjectInputChanged { .. } => true,
                    GraphError::Vault(error) => vault_drift(error),
                    _ => false,
                })
    }

    #[derive(Serialize)]
    struct JobResult {
        operation: Operation,
        ok: bool,
        cancelled: bool,
        #[serde(flatten)]
        halt: HaltState,
        duration_ms: u64,
        semantic_duration_ms: u64,
        execution: Option<ExecutionInfo>,
        workflow_id: Option<lk_core::execution::WorkflowId>,
        semantic_calls: usize,
        recovered_section_proposals: usize,
        budget_exhausted: bool,
        /// Every host configuration that answered nothing in this job reached
        /// `HOST_FAILURE_LIMIT`, so no host remains admissible; its campaign starts no later
        /// batch.
        #[serde(skip)]
        host_fault: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        yield_reason: Option<YieldReason>,
        #[serde(skip_serializing_if = "Option::is_none")]
        automation: Option<automation::Report>,
        #[serde(flatten)]
        work_telemetry: WorkTelemetry,
        queue_planning: QueuePlanningMetrics,
        mutation_access: MutationAccessMetrics,
        queue_application: QueueApplicationMetrics,
        audit: Option<Vec<lk_graph::audit::AuditCompletion>>,
        audit_progress: Option<AuditProgress>,
        audit_blocked: Vec<lk_graph::audit::AuditBlockedInput>,
        audit_blocked_count: usize,
        audit_deferred: Vec<DeferredAudit>,
        audit_deferred_count: usize,
        review_health: Option<lk_vault::ReviewHealth>,
        stages: Vec<StageResult>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum CampaignStop {
        BatchLimit,
        Settled,
        Blocked,
        Deferred,
        NoProgress,
        Failed,
        /// Inputs changed under the batch (`HaltKind::InputsChanged`); the next run replans.
        InputsChanged,
        Cancelled,
        AutomationLimit,
        Yielded,
        HostFault,
    }

    impl JobResult {
        fn new(operation: Operation) -> Self {
            Self {
                operation,
                ok: true,
                cancelled: false,
                halt: HaltState::default(),
                duration_ms: 0,
                semantic_duration_ms: 0,
                execution: None,
                workflow_id: None,
                semantic_calls: 0,
                recovered_section_proposals: 0,
                budget_exhausted: false,
                host_fault: false,
                yield_reason: None,
                automation: None,
                work_telemetry: WorkTelemetry::default(),
                queue_planning: QueuePlanningMetrics::default(),
                mutation_access: MutationAccessMetrics::default(),
                queue_application: QueueApplicationMetrics::default(),
                audit: None,
                audit_progress: None,
                audit_blocked: Vec::new(),
                audit_blocked_count: 0,
                audit_deferred: Vec::new(),
                audit_deferred_count: 0,
                review_health: None,
                stages: Vec::new(),
            }
        }

        fn is_halted(&self) -> bool {
            self.halt.0.is_some()
        }

        /// Records `name`'s failure as the halt that stops this job. The first halt is kept,
        /// except that a fault replaces input drift: a scheduled pause names a fault.
        fn halt_on(&mut self, name: &str, kind: HaltKind, detail: Option<&str>) {
            if self
                .halt
                .0
                .as_ref()
                .is_none_or(|halt| halt.kind == HaltKind::InputsChanged && kind == HaltKind::Fault)
            {
                self.halt.0 = Some(Halt {
                    kind,
                    cause: automation::HaltCause::new(name, detail),
                });
            }
        }

        /// The drift that stopped this job when it retained no semantic progress (no retained
        /// progress and no recorded assessment): what `automation::Control::conclude` counts.
        fn unproductive_drift(&self) -> Option<&automation::HaltCause> {
            let progressed = self.work_telemetry.retained_progress.has_progress()
                || self
                    .audit
                    .as_ref()
                    .is_some_and(|completed| !completed.is_empty());
            self.halt
                .0
                .as_ref()
                .filter(|halt| halt.kind == HaltKind::InputsChanged && !progressed)
                .map(|halt| &halt.cause)
        }

        /// The cause a scheduled operation pauses on: a fault, never input drift.
        fn pause_cause(&self) -> Option<&automation::HaltCause> {
            self.halt
                .0
                .as_ref()
                .filter(|halt| halt.kind == HaltKind::Fault)
                .map(|halt| &halt.cause)
        }

        fn campaign_stop(&self) -> CampaignStop {
            if self.cancelled {
                return CampaignStop::Cancelled;
            }
            if self.host_fault {
                return CampaignStop::HostFault;
            }
            if !self.is_halted()
                && let Some(stop) = self.automation.as_ref().and_then(|report| report.stop)
            {
                return match stop {
                    automation::Stop::Paused => CampaignStop::Blocked,
                    automation::Stop::DayLimit
                    | automation::Stop::RunLimit
                    | automation::Stop::RunDuration
                    | automation::Stop::WaitDuration => CampaignStop::AutomationLimit,
                };
            }
            if let Some(halt) = &self.halt.0 {
                return match halt.kind {
                    HaltKind::Fault => CampaignStop::Failed,
                    HaltKind::InputsChanged => CampaignStop::InputsChanged,
                };
            }
            if self.yield_reason.is_some() {
                return if self.work_telemetry.retained_progress.has_progress() {
                    CampaignStop::Yielded
                } else {
                    CampaignStop::NoProgress
                };
            }
            if matches!(self.operation, Operation::Process) {
                return if self.ok {
                    CampaignStop::Settled
                } else if self.budget_exhausted {
                    if self.work_telemetry.retained_progress.has_progress()
                        || self.recovered_section_proposals != 0
                    {
                        CampaignStop::BatchLimit
                    } else {
                        CampaignStop::NoProgress
                    }
                } else {
                    CampaignStop::Deferred
                };
            }
            if self.audit.as_ref().is_some_and(Vec::is_empty) && self.audit_blocked_count > 0 {
                CampaignStop::Blocked
            } else if !self.ok {
                if self
                    .audit
                    .as_ref()
                    .is_some_and(|completed| !completed.is_empty())
                    || self.work_telemetry.retained_progress.has_progress()
                {
                    CampaignStop::BatchLimit
                } else {
                    CampaignStop::Deferred
                }
            } else if self.audit.as_ref().is_some_and(Vec::is_empty)
                && self
                    .audit_progress
                    .as_ref()
                    .and_then(|progress| progress.after.as_ref())
                    .is_some_and(|coverage| coverage.unreviewed == 0 && coverage.changed == 0)
            {
                CampaignStop::Settled
            } else {
                CampaignStop::BatchLimit
            }
        }

        fn is_successful_yield(&self) -> bool {
            self.yield_reason.is_some()
                && !self.is_halted()
                && !self.cancelled
                && self
                    .stages
                    .iter()
                    .all(|stage| matches!(stage.state, StageState::Completed | StageState::Skipped))
        }
    }

    struct Runner<'a> {
        opts: &'a GlobalOptions,
        installation: &'a lk_dist::Installation,
        config: Config,
        config_revision: String,
        configuration_failed: bool,
        root: PathBuf,
        supervisor: &'a mut execution::Supervisor,
        host_output: execution::ChildOutput,
        automation: Option<&'a mut automation::Control>,
        audit_replans: &'a mut AuditReplans,
        finalization_replans: &'a mut FinalizationReplans,
        /// Subjects this scheduled invocation deferred or found removed (`AuditDeferrals`).
        audit_deferrals: &'a mut AuditDeferrals,
        lease: Option<workflow::WorkflowLease>,
        agents: std::collections::BTreeMap<String, PreparedAgent>,
        project_protection: Option<crate::commands::installation::Protection>,
        drain_blocked: bool,
        inspection_deferred: std::collections::BTreeMap<String, String>,
        host_failures: std::collections::BTreeMap<HostKey, usize>,
        /// Host configurations that reached `HOST_FAILURE_LIMIT`; this run sends them nothing
        /// more, while other hosts keep their work.
        tripped_hosts: std::collections::BTreeSet<HostKey>,
        /// Profiles whose host environment cannot start in this run's job, with why; this run
        /// sends them nothing.
        unavailable_profiles: std::collections::BTreeMap<String, String>,
        /// Host configurations that answered a call in this run.
        answered_hosts: std::collections::BTreeSet<HostKey>,
        /// Hosts this run invoked; a host it already reached needs no probe.
        hosts_invoked: crate::commands::work::host::InvokedHosts,
        /// The probe each drain of this run is choosing, once it found no admissible work.
        host_probes: HostProbes,
        /// The drains whose first turn this run already decided (`take_recovery_turn`).
        recovery_turns: std::collections::BTreeSet<crate::commands::work::host::ProbeScope>,
        audit_deferred_paths: std::collections::BTreeSet<String>,
        audit_batch: Option<Vec<lk_graph::audit::AuditCandidate>>,
        /// Bounded audit subjects whose wave replies found their Work's inputs changed; the
        /// bounded route replans each as that subject's own step would.
        audit_wave_replans: std::collections::BTreeSet<String>,
        queue_turn: Option<QueueTurn>,
        /// The audit subject selected for a single bounded step and the keys it was ordered
        /// among; its cursor turn is reserved with its call (`reserve_turn`).
        audit_turn: Option<(std::collections::BTreeSet<String>, String)>,
        /// Targets of the turn reserved for the current host invocation. Reservation consumes
        /// `queue_turn`; a finalization whose inputs changed afterwards charges these targets.
        reserved_targets: Vec<String>,
        queue_admitted: scheduler::AdmittedTurns,
        queue_replan: QueueReplan,
        writer_access_failed: bool,
        started_at: Instant,
        result: JobResult,
    }

    impl Runner<'_> {
        fn finish(mut self) -> JobResult {
            if !self.result.cancelled
                && self.automation.is_some()
                && self.lease.is_some()
                && self.configuration_is_current()
            {
                let root = self
                    .lease
                    .as_ref()
                    .expect("owned workflow")
                    .root_pin()
                    .root();
                let pause = self.result.pause_cause().cloned();
                let drift = self.result.unproductive_drift().cloned();
                let control = self.automation.as_mut().expect("scheduled run");
                let concluded = match pause {
                    Some(cause) => control.pause(root, cause),
                    None => control.conclude(root, drift.as_ref()),
                };
                if let Err(error) = concluded {
                    self.record(
                        "automation pause",
                        StageState::Failed,
                        Some(error.to_string()),
                    );
                }
            }
            self.result.automation = self
                .automation
                .as_ref()
                .map(|control| control.report.clone());
            self.result.duration_ms = self.started_at.elapsed().as_millis() as u64;
            if self.result.yield_reason.is_some() {
                self.result.ok = false;
            }
            self.result
        }

        fn record(&mut self, name: &str, state: StageState, detail: Option<String>) -> bool {
            let success = matches!(state, StageState::Completed | StageState::Skipped);
            self.result.ok &= success;
            self.result.cancelled |= matches!(state, StageState::Cancelled);
            eprintln!(
                "{name}: {}{}",
                match state {
                    StageState::Completed => "completed",
                    StageState::Skipped => "skipped",
                    StageState::Failed => "failed",
                    StageState::Cancelled => "cancelled",
                },
                detail
                    .as_ref()
                    .map(|value| format!(" — {value}"))
                    .unwrap_or_default()
            );
            self.result.stages.push(StageResult {
                name: name.into(),
                state,
                detail,
                elapsed_job_ms: self.started_at.elapsed().as_millis() as u64,
            });
            success
        }

        /// Records `name` as the failed stage that stops this job. A failure caused by input
        /// drift (`is_input_drift`) ends the batch without pausing; any other is a fault.
        fn failure(&mut self, name: &str, error: miette::Report) -> bool {
            if self.supervisor.is_cancelled() {
                return self.record(name, StageState::Cancelled, Some(error.to_string()));
            }
            let kind = if is_input_drift(&error) {
                HaltKind::InputsChanged
            } else {
                HaltKind::Fault
            };
            let detail = error.to_string();
            self.result.halt_on(name, kind, Some(&detail));
            self.record(name, StageState::Failed, Some(detail))
        }

        /// Records `name` as a faulted stage that stops this job (`HaltKind::Fault`).
        fn halt(&mut self, name: &str, detail: Option<String>) -> bool {
            self.result
                .halt_on(name, HaltKind::Fault, detail.as_deref());
            self.record(name, StageState::Failed, detail)
        }

        fn record_work_step(&mut self, mut step: WorkStepMetrics) {
            if self.result.yield_reason.is_some() && step.agent_duration_ms.is_none() {
                step.outcome = WorkOutcome::Yielded;
            }
            if self.supervisor.is_cancelled()
                && !matches!(
                    step.outcome,
                    WorkOutcome::Recorded
                        | WorkOutcome::Staged
                        | WorkOutcome::Restaged
                        | WorkOutcome::AlreadyComplete
                )
            {
                step.outcome = WorkOutcome::Cancelled;
            }
            if step.unanswered {
                if let Some(tail) = step
                    .host_failure
                    .as_ref()
                    .and_then(execution::diagnostic::HostFailure::stderr_tail)
                {
                    // The host's own account of an answerless failure, such as a missing
                    // configuration directory, is the cause an operator diagnoses.
                    let profile = step
                        .execution
                        .as_ref()
                        .map_or("unknown", |execution| execution.profile.as_str());
                    self.record(
                        "host failure output",
                        StageState::Completed,
                        Some(format!("profile {profile} stderr: {tail}")),
                    );
                }
                self.count_host_failure(step.host());
            } else if step.agent_duration_ms.is_some()
                && !matches!(step.outcome, WorkOutcome::Cancelled | WorkOutcome::Yielded)
            {
                self.host_failures.remove(&step.host());
                if step.answered {
                    self.answered_hosts.insert(step.host());
                    self.settle_host_fault();
                }
            }
            if let (Some(execution), Some(host)) = (&step.execution, &step.host_end) {
                self.hosts_invoked.insert(execution.engine, &host.profile);
            }
            if step.answered
                && let (Some(execution), Some(host)) = (&step.execution, &step.host_end)
                && let Err(error) = crate::commands::work::host::record_answer(
                    &self.root,
                    execution.engine,
                    &host.profile,
                    host.at,
                )
            {
                self.failure("host answer", error);
            }
            self.result.work_telemetry.record(step);
        }

        /// With no admissible work, releases the oldest pending unanswered hold whose engine
        /// this run has not invoked and which has not answered since, as the probe an operator
        /// would send. Called again after planning still found nothing admissible, the probe
        /// released last could not run: it is withdrawn and the next oldest is tried, until
        /// one admits a call or none remains. Returns whether planning should run again.
        fn release_host_probe(
            &mut self,
            scope: crate::commands::work::host::ProbeScope,
            why: ProbeRelease,
        ) -> bool {
            use crate::commands::work::host;
            let root = self.root.clone();
            let mut probe = self.host_probes.scope(scope).take().unwrap_or_default();
            let withdrawn = probe.released.take();
            probe.target = None;
            let mut remaining: std::collections::VecDeque<_> = match probe.remaining.take() {
                // A candidate whose request settled or whose preparation was published since
                // it was listed is no longer pending; only standing holds are released.
                Some(remaining) => match host::pending_holds(&root) {
                    Ok(pending) => remaining
                        .into_iter()
                        .filter(|hold| pending.iter().any(|current| current.is_same_hold(hold)))
                        .collect(),
                    Err(error) => {
                        self.failure("host probe", error);
                        return false;
                    }
                },
                None => {
                    let candidates = host::pending_holds(&root).and_then(|holds| {
                        host::HostAnswers::load(&root)?.probe_candidates(
                            &root,
                            scope,
                            &self.hosts_invoked,
                            holds,
                            self.supervisor.read_control(),
                            jiff::Timestamp::now(),
                        )
                    });
                    match candidates {
                        // A probe inherited from an earlier run and withdrawn now is not tried
                        // again among the candidates.
                        Ok(candidates) => candidates
                            .into_iter()
                            .filter(|hold| {
                                withdrawn
                                    .as_ref()
                                    .is_none_or(|withdrawn| withdrawn.identity != hold.identity)
                            })
                            .collect(),
                        Err(error) => {
                            self.failure("host probe", error);
                            return false;
                        }
                    }
                }
            };
            let next = loop {
                match remaining.pop_front() {
                    Some(hold) if self.hosts_invoked.reached(hold.engine, hold.host.as_ref()) => {}
                    next => break next,
                }
            };
            probe.remaining = Some(remaining);
            *self.host_probes.scope(scope) = Some(probe);
            if let Err(error) = host::record_probe(&root, next.as_ref()) {
                self.failure("host probe", error);
                return false;
            }
            let Some(hold) = next else {
                return false;
            };
            if let Err(error) = host::record_recovery_due(&root, scope, false) {
                self.failure("host probe", error);
                return false;
            }
            let target = probe_hold_target(&root, &hold);
            self.record(
                "host probe",
                StageState::Completed,
                Some(format!(
                    "{}; released the oldest pending unanswered hold {} as a probe of its {} host",
                    match why {
                        ProbeRelease::Idle => "no work was admissible",
                        ProbeRelease::RecoveryTurn => "the previous run gave its turn to other work while this hold waited, so recovery goes first",
                        ProbeRelease::Unplannable => "the hold released before cannot be planned now",
                    },
                    serde_json::to_string(&hold.identity).expect("hold identity serializes"),
                    hold.host
                        .as_ref()
                        .map(|host| host.profile.as_str())
                        .or(hold.engine.map(|engine| engine.as_str()))
                        .unwrap_or("first answering"),
                )),
            );
            // The vault keeps one probe, so this release replaces the other drain's and any
            // earlier run's.
            for drain in [host::ProbeScope::Queue, host::ProbeScope::Audit] {
                if let Some(probe) = self.host_probes.scope(drain) {
                    probe.released = None;
                }
            }
            let released = self
                .host_probes
                .scope(scope)
                .as_mut()
                .expect("probe candidates");
            released.released = Some(hold);
            released.target = target;
            self.queue_replan = QueueReplan::Invalidate;
            true
        }

        /// Takes the probe an earlier run released, while its hold still stands released, as
        /// its drain's released probe: it goes first, or is withdrawn when planning cannot take
        /// it, exactly like one released now. A ledger probe whose hold was settled or renewed
        /// is retired. Returns whether the run continues.
        fn inherit_host_probe(&mut self) -> bool {
            use crate::commands::work::host;
            let root = self.root.clone();
            let inherited = host::HostAnswers::load(&root).and_then(|answers| {
                Ok((
                    answers.has_probe(),
                    answers.inherited_probe(host::pending_holds(&root)?),
                ))
            });
            match inherited {
                Ok((_, Some(hold))) => {
                    let target = probe_hold_target(&root, &hold);
                    let drain = hold.scope.drain();
                    *self.host_probes.scope(drain) = Some(HostProbe {
                        remaining: None,
                        released: Some(hold),
                        target,
                    });
                    true
                }
                Ok((true, None)) => match host::record_probe(&root, None) {
                    Ok(()) => true,
                    Err(error) => self.failure("host probe", error),
                },
                Ok((false, None)) => true,
                Err(error) => self.failure("host probe", error),
            }
        }

        /// Retires `scope`'s released probe once its request settled, by whichever profile
        /// answered it: its hold is no longer pending, so it neither goes first nor holds waves
        /// to one call. Returns whether a released probe remains.
        fn retire_settled_probe(&mut self, scope: crate::commands::work::host::ProbeScope) -> bool {
            use crate::commands::work::host;
            let Some(released) = self
                .host_probes
                .scope(scope)
                .as_ref()
                .and_then(|probe| probe.released.clone())
            else {
                return false;
            };
            let root = self.root.clone();
            let pending = match host::pending_holds(&root) {
                Ok(holds) => holds.iter().any(|hold| hold.is_same_hold(&released)),
                Err(error) => {
                    self.failure("host probe", error);
                    return false;
                }
            };
            if pending {
                return true;
            }
            if let Some(probe) = self.host_probes.scope(scope) {
                probe.released = None;
                probe.target = None;
            }
            if let Err(error) = host::record_probe(&root, None) {
                self.failure("host probe", error);
            }
            false
        }

        /// The target of `scope`'s released probe while its host has given no outcome in this
        /// run: the drain sends it before other work.
        fn probe_target(
            &mut self,
            scope: crate::commands::work::host::ProbeScope,
        ) -> Option<String> {
            let probe = self.host_probes.scope(scope).as_ref()?;
            let hold = probe.released.as_ref()?;
            if self.hosts_invoked.reached(hold.engine, hold.host.as_ref()) {
                return None;
            }
            probe.target.clone()
        }

        /// Whether `scope`'s released probe, while its host has given no outcome in this run,
        /// is missing from the work just planned: its target is no longer current, waits on an
        /// unfinished dependency, or falls outside the drain's admission. Such a probe is
        /// withdrawn for the next candidate rather than left waiting behind ordinary work.
        fn is_probe_unplanned(
            &mut self,
            scope: crate::commands::work::host::ProbeScope,
            planned: impl Fn(&str) -> bool,
        ) -> bool {
            if !self.retire_settled_probe(scope) {
                return false;
            }
            let Some(probe) = self.host_probes.scope(scope).as_ref() else {
                return false;
            };
            let Some(hold) = probe.released.as_ref() else {
                return false;
            };
            !self.hosts_invoked.reached(hold.engine, hold.host.as_ref())
                && probe
                    .target
                    .as_deref()
                    .is_none_or(|target| !planned(target))
        }

        /// Decides, once per run and drain, who takes the first turn. When the previous run
        /// gave it to ordinary work while probes waited, a probe is released now and sent
        /// first; otherwise ordinary work goes first and, if probes wait, recovery is owed the
        /// next run's first turn. Either side thus waits at most one run, even at one call per
        /// run.
        fn take_recovery_turn(&mut self, scope: crate::commands::work::host::ProbeScope) {
            use crate::commands::work::host;
            if !self.recovery_turns.insert(scope) {
                return;
            }
            let root = self.root.clone();
            let due = match host::HostAnswers::load(&root) {
                Ok(answers) => answers.is_recovery_due(scope),
                Err(error) => {
                    self.failure("host probe", error);
                    return;
                }
            };
            if due
                && self
                    .host_probes
                    .scope(scope)
                    .as_ref()
                    .is_some_and(|probe| probe.released.is_some())
            {
                // A probe inherited from an earlier run already goes first in this drain.
                if let Err(error) = host::record_recovery_due(&root, scope, false) {
                    self.failure("host probe", error);
                }
                return;
            }
            if due {
                self.release_host_probe(scope, ProbeRelease::RecoveryTurn);
                return;
            }
            let waiting = host::pending_holds(&root).and_then(|holds| {
                host::HostAnswers::load(&root)?.probe_candidates(
                    &root,
                    scope,
                    &self.hosts_invoked,
                    holds,
                    self.supervisor.read_control(),
                    jiff::Timestamp::now(),
                )
            });
            match waiting {
                Ok(waiting) if !waiting.is_empty() => {
                    if let Err(error) = host::record_recovery_due(&root, scope, true) {
                        self.failure("host probe", error);
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    self.failure("host probe", error);
                }
            }
        }

        /// The host configuration that served a step whose host ended at `finished_at`.
        fn host_end(
            metrics: &WorkStepMetrics,
            finished_at: jiff::Timestamp,
        ) -> Option<crate::commands::work::state::HostEnd> {
            metrics
                .execution
                .as_ref()
                .map(|execution| crate::commands::work::state::HostEnd {
                    profile: execution.profile.clone(),
                    at: finished_at,
                    probe_failures: 0,
                })
        }

        /// The calls a wave may send at once. While the host of a released probe, this run's or
        /// one an earlier run released, has given no outcome in this run, one call learns
        /// whether the host serves requests.
        fn wave_limit(&self, budget: lk_core::execution::SemanticWorkConfig) -> usize {
            let limit = budget.max_parallel.min(self.remaining_calls(budget));
            let probing = self.host_probes.released().any(|(engine, profile)| {
                !self.hosts_invoked.reached_profile(engine, profile.as_ref())
            });
            if probing { limit.min(1) } else { limit }
        }

        /// Counts an invocation that ended without an answer. At `HOST_FAILURE_LIMIT` for one
        /// host configuration the run sends that configuration no further call and defers the
        /// work routed to it; other configurations keep calling and returned results keep
        /// publishing.
        fn count_host_failure(&mut self, host: HostKey) {
            let count = self.host_failures.entry(host.clone()).or_default();
            *count += 1;
            if *count < HOST_FAILURE_LIMIT || !self.tripped_hosts.insert(host.clone()) {
                return;
            }
            let count = *count;
            self.settle_host_fault();
            let (role, channel, profile) = host;
            let name = |value: serde_json::Value| value.as_str().unwrap_or_default().to_owned();
            self.deferred(
                "host failures",
                format!(
                    "{count} consecutive {} invocations through the {} host on profile {} failed without an answer; the run sends that host no further call so a shared host fault spends nothing more, defers its work and continues on other hosts",
                    name(serde_json::to_value(role).expect("role serializes")),
                    name(serde_json::to_value(channel).expect("channel serializes")),
                    profile.as_deref().unwrap_or("unknown")
                ),
            );
        }

        /// Whether no host remains admissible: some host tripped and none that did not trip
        /// answered in this run.
        fn settle_host_fault(&mut self) {
            self.result.host_fault = !self.tripped_hosts.is_empty()
                && self
                    .answered_hosts
                    .iter()
                    .all(|host| self.tripped_hosts.contains(host));
        }

        /// Sends nothing in this run to the profiles whose host environment cannot start, and
        /// names them in a failed `host environment` stage, so the job reports itself
        /// incomplete. Their work is deferred without a hold or a pause; other profiles keep
        /// calling and returned results keep publishing, and the next run checks again.
        fn exclude_unavailable_hosts(
            &mut self,
            unavailable: std::collections::BTreeMap<String, String>,
        ) {
            if unavailable.is_empty() {
                return;
            }
            let detail = unavailable
                .iter()
                .map(|(profile, reason)| format!("profile {profile}: {reason}"))
                .collect::<Vec<_>>()
                .join("; ");
            self.record(
                automation::HOST_ENVIRONMENT_STAGE,
                StageState::Failed,
                Some(format!(
                    "{detail}; work routed to these profiles is deferred to a later run, which checks again"
                )),
            );
            self.unavailable_profiles = unavailable;
        }

        /// The error of a call whose host could not start (no executable, a start failure, a
        /// failed or unsupported `--version`). A scheduled run excludes the profile for the
        /// rest of the run, as its preflight does, and the next run checks again; an attended
        /// run keeps the error.
        fn unstartable_host(&mut self, profile: &str, error: miette::Report) -> miette::Report {
            if self.automation.is_none() {
                return error;
            }
            let reason = error.to_string();
            self.record(
                automation::HOST_ENVIRONMENT_STAGE,
                StageState::Failed,
                Some(format!(
                    "profile {profile}: {reason}; work routed to it is deferred to a later run, which checks again"
                )),
            );
            let unavailable = host_unavailable(profile, &reason);
            self.unavailable_profiles.insert(profile.to_owned(), reason);
            unavailable.into()
        }

        /// The deferral of a scheduled call whose host could not start (`ChildStartFailure`)
        /// after its version check passed, as when its executable or interpreter went away:
        /// its profile is excluded for the rest of the run like one that cannot start at its
        /// first call, and the step is unsent. Any other error, and every error of an attended
        /// run, is returned as it is.
        fn unstarted_call(
            &mut self,
            metrics: &mut WorkStepMetrics,
            error: miette::Report,
        ) -> Result<String, miette::Report> {
            if error
                .downcast_ref::<execution::ChildStartFailure>()
                .is_none()
            {
                return Err(error);
            }
            let Some(profile) = metrics
                .execution
                .as_ref()
                .map(|execution| execution.profile.clone())
            else {
                return Err(error);
            };
            let error = self.unstartable_host(&profile, error);
            match error.downcast_ref::<HostUnavailable>() {
                Some(unavailable) => {
                    metrics.outcome = WorkOutcome::Unsent;
                    Ok(unavailable.to_string())
                }
                None => Err(error),
            }
        }

        /// The deferral of a call routed to a profile whose host environment cannot start in
        /// this run, if it is one.
        fn unavailable_profile(&self, profile: &str) -> Option<HostUnavailable> {
            self.unavailable_profiles
                .get(profile)
                .map(|reason| host_unavailable(profile, reason))
        }

        /// The deferral of a call routed to a host this run tripped, if it is one.
        fn tripped_host(&self, metrics: &WorkStepMetrics) -> Option<HostUnavailable> {
            let host = metrics.host();
            self.tripped_hosts.contains(&host).then(|| {
                HostUnavailable(format!(
                    "profile {} failed {HOST_FAILURE_LIMIT} consecutive calls without an answer in this run; its work is deferred to a later run",
                    host.2.as_deref().unwrap_or("unknown")
                ))
            })
        }

        /// Defers the current queue turn's targets for the rest of this run because their host
        /// tripped; nothing is held, and planning continues with other targets.
        fn defer_turn_for_host(&mut self, reason: &str) -> WorkStep {
            let targets = self
                .queue_turn
                .as_ref()
                .map(|turn| turn.targets.clone())
                .unwrap_or_else(|| self.reserved_targets.clone());
            for target in &targets {
                self.finalization_replans.defer(target, reason);
            }
            self.replan_after_hold();
            self.deferred(
                "host unavailable",
                format!("{}: {reason}", targets.join(", ")),
            );
            WorkStep::Deferred(reason.into())
        }

        /// Defers audit subjects for the rest of this run because their host tripped; nothing
        /// is held.
        fn defer_audit_for_host<'a>(
            &mut self,
            paths: impl IntoIterator<Item = &'a str>,
            reason: &str,
        ) {
            let paths = paths.into_iter().collect::<Vec<_>>();
            for path in &paths {
                self.audit_deferrals
                    .deferred
                    .insert((*path).to_owned(), reason.to_owned());
            }
            self.deferred(
                "host unavailable",
                format!("{}: {reason}", paths.join(", ")),
            );
        }

        /// Discards queue planning after a request was held: the next plan skips it and
        /// continues with independent work.
        fn replan_after_hold(&mut self) {
            self.queue_replan = QueueReplan::Invalidate;
            self.queue_admitted.clear();
        }

        fn deferred(&mut self, name: &str, detail: impl std::fmt::Display) -> bool {
            self.record(name, StageState::Failed, Some(detail.to_string()))
        }

        fn configuration_is_current(&mut self) -> bool {
            if self.supervisor.is_cancelled() {
                if !self.result.cancelled {
                    self.record(
                        "cancellation",
                        StageState::Cancelled,
                        Some("interrupted".into()),
                    );
                }
                return false;
            }
            if self.configuration_failed {
                return false;
            }
            let pin = crate::commands::ConfigurationPin::from_loaded(
                self.opts.config.as_ref().expect("resolved config").clone(),
                self.config_revision.clone(),
            );
            let failure = match pin.validate() {
                Ok(()) => {
                    let root = if let Some(lease) = &self.lease {
                        lease.root_pin().validate()
                    } else if lk_core::fs::canonical_prefix(&self.config.vault.root_path())
                        == self.root
                    {
                        Ok(())
                    } else {
                        Err(miette::miette!(
                            "workflow vault selection changed; unfinished work is retained"
                        ))
                    };
                    match root {
                        Ok(()) => return true,
                        Err(error) => error.to_string(),
                    }
                }
                Err(error) => error.to_string(),
            };
            self.configuration_failed = true;
            self.failure("configuration", miette::miette!("{}", failure))
        }

        async fn child(
            &mut self,
            name: &str,
            command: tokio::process::Command,
            timeout: Option<Duration>,
        ) -> bool {
            self.child_with_input(name, command, timeout, None).await
        }

        async fn child_with_input(
            &mut self,
            name: &str,
            mut command: tokio::process::Command,
            timeout: Option<Duration>,
            input: Option<&[u8]>,
        ) -> bool {
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return false;
            }
            self.configure_child(&mut command);
            let completed = self
                .supervisor
                .run_request(
                    command,
                    timeout,
                    None,
                    input,
                    execution::ChildOutput::Forward,
                )
                .await;
            self.settle_child(name, completed)
        }

        fn settle_child(
            &mut self,
            name: &str,
            completed: miette::Result<execution::ChildResult>,
        ) -> bool {
            let completed = match completed {
                Ok(result) if result.cancelled => {
                    self.record(name, StageState::Cancelled, result.detail)
                }
                Ok(result) if result.success => {
                    self.record(name, StageState::Completed, result.detail)
                }
                Ok(result) if self.lease.is_some() => self.halt(name, result.detail),
                Ok(result) => self.record(name, StageState::Failed, result.detail),
                Err(error) => self.failure(name, error),
            };
            self.configuration_is_current() && completed
        }

        fn configure_child(&self, command: &mut tokio::process::Command) {
            if let Some(lease) = &self.lease {
                lease.configure(command.as_std_mut());
            }
            command.env(CONFIG_REVISION_ENV, &self.config_revision).env(
                "LORE_CONFIG",
                self.opts.config.as_ref().expect("resolved config"),
            );
            command.env("LORE_INSTALL_DATA_DIR", self.installation.data_dir());
            if let Some(name) = self.config.execution.selected_profile() {
                command.env(EXECUTION_PROFILE_ENV, name);
            } else {
                command.env_remove(EXECUTION_PROFILE_ENV);
            }
        }

        async fn native(&mut self, name: &str, args: &[&str]) -> bool {
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return false;
            }
            if args == ["queue", "apply"] {
                return self.apply_queue(name).await;
            }
            if args == ["queue", "check"] {
                return self.check_queue(name);
            }
            if args == ["wiki", "refresh"] {
                return self.refresh_navigation(name).await;
            }
            let mut command_args = args.to_vec();
            if args == ["schema"] || args.starts_with(&["wiki", "audit-record"]) {
                command_args.push("--wait-for-vault");
            }
            match execution::lore_command(self.opts, &command_args) {
                Ok(command) => self.child(name, command, None).await,
                Err(error) => self.failure(name, error),
            }
        }

        /// Navigation refresh stays a child process so cancellation can stop its long capture.
        /// An external edit during that capture defers it: the previous snapshot stays, a later
        /// run refreshes, and independent work continues without halting or pausing.
        async fn refresh_navigation(&mut self, name: &str) -> bool {
            let mut command = match execution::lore_command(
                self.opts,
                &["wiki", "refresh", "--wait-for-vault"],
            ) {
                Ok(command) => command,
                Err(error) => return self.failure(name, error),
            };
            self.configure_child(&mut command);
            let completed = self
                .supervisor
                .run_request(command, None, None, None, execution::ChildOutput::Forward)
                .await;
            match completed {
                Ok(result)
                    if !result.cancelled
                        && result.exit_code
                            == Some(crate::commands::wiki::REFRESH_CAPTURE_CHANGED_EXIT) =>
                {
                    self.record(
                        name,
                        StageState::Failed,
                        Some("navigation capture could not complete (the vault changed while it read, or another connection held the search index); the previous snapshot stays and a later run refreshes it".into()),
                    );
                    self.configuration_is_current()
                }
                completed => self.settle_child(name, completed),
            }
        }

        /// Backlink reconciliation inside the run. Its planner takes the pages this run already
        /// read while their files keep their stamps, and publication guards their current bytes.
        /// A concept page that cannot record its derived evidence leaves the job incomplete while
        /// independent work continues; failed reads and publication conflicts stop the job.
        async fn backlinks_sync(&mut self, name: &str) -> bool {
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return false;
            }
            let Some((session, _)) = self.mutation_session(name).await else {
                return false;
            };
            let synchronized = crate::commands::graph::backlinks_sync_in_session(
                &self.config,
                &session,
                self.supervisor.read_control(),
            )
            .and_then(|report| {
                crate::commands::graph::request_review_resyntheses_in_session(
                    &self.config,
                    &session,
                    self.supervisor.read_control(),
                )
                .map_err(miette::Report::msg)
                .map(|closed| (report, closed))
            });
            drop(session);
            if !self.configuration_is_current() {
                return false;
            }
            let (report, review_callouts) = match synchronized {
                Ok(synchronized) => synchronized,
                Err(error) => return self.failure(name, error),
            };
            let mut detail = format!(
                "{} updated, {} queued, {} owed a synthesis ({} replacing written sections)",
                report.changed,
                report.queued,
                report.sync.resynthesize.len(),
                report
                    .sync
                    .resynthesize
                    .iter()
                    .filter(|owed| !owed.discarding.is_empty())
                    .count()
            );
            if !review_callouts.requested.is_empty() {
                detail.push_str(&format!(
                    "; {} synthesis request(s) for concepts whose open reviews changed since their synthesis",
                    review_callouts.requested.len()
                ));
            }
            if !review_callouts.unresynthesizable.is_empty() {
                detail.push_str(&format!(
                    "; {} review callout(s) on concepts that cannot be resynthesized (no current citations or synthesis heading)",
                    review_callouts.unresynthesizable.len()
                ));
            }
            if !review_callouts.unrecorded.is_empty() {
                detail.push_str(&format!(
                    "; {} gap callout(s) without a publication record are left as written; `lore graph resynthesize` brings one under review tracking",
                    review_callouts.unrecorded.len()
                ));
            }
            if !review_callouts.invalid.is_empty() {
                detail.push_str(&format!(
                    "; {} concept(s) with an unreadable review record keep their synthesis until it is repaired: {}",
                    review_callouts.invalid.len(),
                    review_callouts
                        .invalid
                        .iter()
                        .take(3)
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !review_callouts.held.is_empty() {
                detail.push_str(&format!(
                    "; {} concept(s) whose own audit records cannot be read are not checked against their reviews: {}",
                    review_callouts.held.len(),
                    review_callouts
                        .held
                        .iter()
                        .take(3)
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let skipped = &report.sync.skipped;
            if skipped.is_empty() {
                let state = if review_callouts.invalid.is_empty() && review_callouts.held.is_empty()
                {
                    StageState::Completed
                } else {
                    StageState::Failed
                };
                return self.record(name, state, Some(detail));
            }
            let shown = skipped
                .iter()
                .take(3)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let more = match skipped.len().saturating_sub(3) {
                0 => String::new(),
                more => format!(" and {more} more"),
            };
            self.record(
                name,
                StageState::Failed,
                Some(format!(
                    "{detail}; {} concept page(s) cannot record derived evidence: {shown}{more}; inspect `lore graph backlinks-sync --dry-run`",
                    skipped.len()
                )),
            );
            true
        }

        /// Structural lint inside the run. A violation names the pages it concerns, so it leaves
        /// the job incomplete without stopping independent work; a check that could not complete
        /// stops the job.
        fn graph_lint(&mut self, name: &str) -> bool {
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return false;
            }
            let linted = crate::commands::graph::lint_controlled(
                &self.config,
                &self.root,
                self.supervisor.read_control(),
            );
            if !self.configuration_is_current() {
                return false;
            }
            let report = match linted {
                Ok(report) => report,
                Err(error) => return self.failure(name, miette::miette!("{}", error)),
            };
            if !report.errors.is_empty() {
                return self.halt(name, Some(report.errors.join("; ")));
            }
            let violations = report.violations.count();
            let detail = format!(
                "{} pages, {} links, {violations} violation(s)",
                report.pages, report.links
            );
            if violations == 0 {
                return self.record(name, StageState::Completed, Some(detail));
            }
            self.record(
                name,
                StageState::Failed,
                Some(format!("{detail}; inspect `lore graph lint`")),
            );
            true
        }

        fn check_queue(&mut self, name: &str) -> bool {
            let state = match crate::commands::queue::check::inspect_controlled(
                &self.root,
                self.supervisor.read_control(),
            ) {
                Ok(state) => state,
                Err(error) => return self.failure(name, error),
            };
            if state.pending_mutation || state.unreadable != 0 || state.unparseable_lines != 0 {
                return self.failure(name, miette::miette!("queue contains an interrupted mutation or unknown inputs; inspect `lore queue check`"));
            }
            if !state.staged_results.is_empty()
                && let Err(error) =
                    crate::commands::queue::readiness::InputReadiness::load_controlled(
                        &self.root,
                        self.supervisor.read_control(),
                    )
            {
                return self.failure(name, error);
            }
            if state.is_settled() {
                return self.record(name, StageState::Completed, None);
            }
            self.deferred(
                name,
                format!(
                    "{} current tasks, {} invalid targets and {} staged results remain; inspect `lore queue check`",
                    state.current,
                    state.invalid_target,
                    state.staged_results.len()
                ),
            )
        }

        async fn apply_queue(&mut self, name: &str) -> bool {
            let mut command = match execution::lore_command(
                self.opts,
                &["queue", "apply", "--json", "--wait-for-vault"],
            ) {
                Ok(command) => command,
                Err(error) => return self.failure(name, error),
            };
            self.configure_child(&mut command);
            let started_at = Instant::now();
            self.result.queue_application.attempts += 1;
            let outcome = self
                .supervisor
                .run_with_output(
                    command,
                    None,
                    Some(8 * 1024 * 1024),
                    execution::ChildOutput::Forward,
                )
                .await;
            self.result.queue_application.duration_ms += started_at.elapsed().as_millis() as u64;
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(error) => return self.failure(name, error),
            };
            if outcome.cancelled {
                self.record(name, StageState::Cancelled, outcome.detail);
                return false;
            }
            if !self.configuration_is_current() {
                return false;
            }
            let report: crate::commands::queue::application::Report =
                match serde_json::from_slice(&outcome.stdout) {
                    Ok(report) => report,
                    Err(error) => {
                        return self.failure(
                            name,
                            miette::miette!(
                                "queue application report is unavailable or invalid: {error}"
                            ),
                        );
                    }
                };
            if outcome.success != report.ok || outcome.exit_code != Some(i32::from(!report.ok)) {
                return self.failure(
                    name,
                    miette::miette!("queue application report disagrees with its process exit"),
                );
            }
            let can_continue = report.can_continue();
            self.result.queue_application.applied += report.applied;
            self.result
                .work_telemetry
                .retained_progress
                .queue_publication_observed |= report.applied != 0;
            self.result.queue_application.dropped += report.dropped;
            self.result.queue_application.rejected += report.rejected;
            self.result.queue_application.orphaned += report.orphaned;
            self.result.queue_application.superseded += report.superseded;
            self.result.queue_application.held += report.held;
            for group in &report.groups {
                if let (
                    crate::commands::queue::application::Outcome::Superseded { .. },
                    Some(identity),
                ) = (&group.outcome, &group.identity)
                    && !self
                        .finalization_replans
                        .try_spend(&identity.target.vault_path, FINALIZATION_CHANGED_AGAIN)
                {
                    self.deferred("finalization inputs unstable", &identity.target.vault_path);
                }
            }
            let detail = format!(
                "{} applied, {} dropped, {} rejected, {} orphaned, {} superseded, {} held; {}",
                report.applied,
                report.dropped,
                report.rejected,
                report.orphaned,
                report.superseded,
                report.held,
                if can_continue {
                    "independent work may continue"
                } else {
                    "unsafe or unknown state stops further work"
                }
            );
            let success = report.ok;
            self.result.queue_application.latest = Some(report);
            if !can_continue {
                self.halt(name, Some(detail));
            } else if success {
                self.record(name, StageState::Completed, Some(detail));
            } else {
                self.record(name, StageState::Failed, Some(detail));
            }
            can_continue
        }

        async fn acquire(&mut self, wait: bool) -> bool {
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return false;
            }
            if let Some(control) = &mut self.automation {
                match control.preflight(&self.root) {
                    Ok(true) => {}
                    Ok(false) => {
                        return self.configuration_is_current() && self.record_automation_stop();
                    }
                    Err(error) => return self.failure("automation preflight", error),
                }
            }
            let wait_deadline = if let Some(control) = &mut self.automation {
                match control.wait_deadline() {
                    Ok(Some(deadline)) => Some(deadline),
                    Ok(None) => return self.record_automation_stop(),
                    Err(error) => return self.failure("workflow waiting", error),
                }
            } else {
                None
            };
            let acquisition = self.supervisor.acquire(
                self.opts,
                &self.config,
                &self.root,
                &self.config_revision,
                wait,
            );
            let acquired = if let Some(deadline) = wait_deadline {
                let started = Instant::now();
                let acquired = tokio::time::timeout_at(deadline.into(), acquisition).await;
                let control = self.automation.as_mut().expect("scheduled run");
                if let Err(error) = control.record_wait(started.elapsed()) {
                    return self.failure("workflow waiting", error);
                }
                match acquired {
                    Ok(acquired) => acquired,
                    Err(_) => {
                        self.automation
                            .as_mut()
                            .expect("scheduled run")
                            .stop_waiting();
                        return self.record_automation_stop();
                    }
                }
            } else {
                acquisition.await
            };
            match acquired {
                Ok(Some(lease)) => {
                    self.root = lease.root_pin().root().to_path_buf();
                    self.config.vault.root = self.root.clone();
                    self.result.workflow_id = Some(lease.id().clone());
                    self.lease = Some(lease);
                    if !self.inherit_host_probe() {
                        return false;
                    }
                    if let Some(control) = &mut self.automation {
                        match control.enter(&self.root) {
                            Ok(true) => {}
                            Ok(false) => return self.record_automation_stop(),
                            Err(error) => return self.failure("automation admission", error),
                        }
                    }
                    self.configuration_is_current()
                }
                Ok(None) => self.record(
                    "workflow lease",
                    StageState::Cancelled,
                    Some("cancelled while waiting".into()),
                ),
                Err(error)
                    if self.automation.is_some()
                        && error.downcast_ref::<workflow::WorkflowBusy>().is_some() =>
                {
                    self.deferred(
                        "workflow lease",
                        "busy; retained for a later scheduled invocation",
                    )
                }
                Err(error) => self.failure("workflow lease", error),
            }
        }

        async fn mutation_session(
            &mut self,
            name: &str,
        ) -> Option<(lk_vault::MutationSession, bool)> {
            if self.result.is_halted() {
                return None;
            }
            self.recording_session(name).await
        }

        async fn recording_session(
            &mut self,
            name: &str,
        ) -> Option<(lk_vault::MutationSession, bool)> {
            if self.writer_access_failed
                || self.result.cancelled
                || !self.configuration_is_current()
            {
                return None;
            }
            let pin = crate::commands::ConfigurationPin::from_loaded(
                self.opts.config.as_ref().expect("resolved config").clone(),
                self.config_revision.clone(),
            );
            let root_pin = self.lease.as_ref().map(|lease| lease.root_pin().clone());
            self.result.mutation_access.attempts += 1;
            let started_at = Instant::now();
            let acquired = self
                .supervisor
                .acquire_mutation(&self.root, move || {
                    pin.validate()?;
                    if let Some(root) = &root_pin {
                        root.validate()?;
                    }
                    Ok(())
                })
                .await;
            self.result.mutation_access.duration_ms += started_at.elapsed().as_millis() as u64;
            match acquired {
                Ok(Some(acquired)) => Some(acquired),
                Ok(None) => {
                    self.record(
                        name,
                        StageState::Cancelled,
                        Some("cancelled while waiting for vault mutation access".into()),
                    );
                    None
                }
                Err(error) => {
                    self.writer_access_failed = true;
                    if self.configuration_is_current() {
                        self.failure(name, error);
                    }
                    None
                }
            }
        }

        async fn prepare_agent(&mut self, metrics: &mut WorkStepMetrics) -> miette::Result<()> {
            if self.project_protection.is_none() {
                self.project_protection =
                    Some(crate::commands::installation::Protection::native_project(
                        self.installation,
                        self.opts,
                        &self.root,
                    )?);
            }
            if metrics.stages.is_empty() {
                metrics
                    .stages
                    .push(match (metrics.role, metrics.node_kind) {
                        (WorkRole::Evidence, Some(NodeKind::Inspect)) => ExecutionStage::Inspect,
                        (WorkRole::Evidence, Some(NodeKind::Reduce)) => ExecutionStage::Reduce,
                        (WorkRole::Screening, _) => ExecutionStage::Screening,
                        (WorkRole::Finalizer, _) => ExecutionStage::Section,
                        _ => {
                            return Err(miette::miette!(
                                "agent invocation has no typed execution stage"
                            ));
                        }
                    });
            }
            let (name, profile) = self
                .config
                .execution
                .resolve_stage_profile(metrics.stages[0])
                .map_err(typed_error)?;
            for stage in &metrics.stages[1..] {
                let (other, _) = self
                    .config
                    .execution
                    .resolve_stage_profile(*stage)
                    .map_err(typed_error)?;
                if name != other {
                    return Err(miette::miette!(
                        "one semantic batch requires the same execution profile"
                    ));
                }
            }
            let name = name.to_owned();
            if let Some(unavailable) = self.unavailable_profile(&name) {
                return Err(unavailable.into());
            }
            if let Some(agent) = self.agents.get(&name) {
                metrics.execution = Some(agent.execution.clone());
                return match self.tripped_host(metrics) {
                    Some(unavailable) => Err(unavailable.into()),
                    None => Ok(()),
                };
            }
            let mut profile = profile.clone();
            let executable = match execution::executable_path(profile.executable()) {
                Ok(executable) => executable,
                Err(error) => return Err(self.unstartable_host(&name, error)),
            };
            profile.set_executable(executable.clone());
            let mut command = tokio::process::Command::new(executable);
            command.arg("--version");
            let version = match self
                .supervisor
                .run(
                    command,
                    Some(Duration::from_secs(10).min(profile.timeout())),
                    true,
                    self.host_output,
                )
                .await
            {
                Ok(version) => version,
                Err(error)
                    if error
                        .downcast_ref::<execution::ChildStartFailure>()
                        .is_some() =>
                {
                    return Err(self.unstartable_host(&name, error));
                }
                Err(error) => return Err(error),
            };
            if version.cancelled {
                self.result.cancelled = true;
            }
            if !version.success {
                let error = miette::miette!(
                    "agent preflight failed: {}",
                    version.detail.as_deref().unwrap_or("version unavailable")
                );
                if version.cancelled {
                    return Err(error);
                }
                metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                    profile.engine(),
                    execution::diagnostic::Phase::VersionPreflight,
                    &version,
                ));
                return Err(self.unstartable_host(&name, error));
            }
            if let Err(error) = execution::verify_version(profile.engine(), &version.stdout) {
                metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                    profile.engine(),
                    execution::diagnostic::Phase::VersionPreflight,
                    &version,
                ));
                return Err(self.unstartable_host(&name, error));
            }
            let host = RequestedExecution {
                profile: name,
                engine: profile.engine(),
                version: String::from_utf8(version.stdout)
                    .map_err(typed_error)?
                    .trim()
                    .into(),
                requested_model: profile.model().map(str::to_owned),
                requested_effort: profile.requested_effort().map(str::to_owned),
            };
            if self.result.execution.is_none() {
                self.result.execution = Some(ExecutionInfo {
                    host: host.clone(),
                    policy_profile: self
                        .config
                        .execution
                        .selected_profile()
                        .expect("selected job profile")
                        .into(),
                    work: self
                        .config
                        .execution
                        .effective_work()
                        .map_err(typed_error)?,
                });
            }
            metrics.execution = Some(host.clone());
            self.agents.insert(
                host.profile.clone(),
                PreparedAgent {
                    profile,
                    execution: host,
                },
            );
            Ok(())
        }

        fn agent_profile(&self, metrics: &WorkStepMetrics) -> &ExecutionProfile {
            &self.agents[&metrics
                .execution
                .as_ref()
                .expect("prepared execution")
                .profile]
                .profile
        }

        fn agent_preflight_failure(&mut self, name: &str, error: miette::Report) -> bool {
            if self.result.cancelled {
                self.record(name, StageState::Cancelled, Some(error.to_string()))
            } else {
                self.failure(name, error)
            }
        }

        fn observation_invocation(
            &self,
            request: &serde_json::Value,
            role: WorkRole,
            metrics: &WorkStepMetrics,
            budget: lk_core::execution::SemanticWorkConfig,
        ) -> miette::Result<(
            execution::observation::Invocation,
            usize,
            Option<crate::commands::work::workload::ObservationWorkload>,
        )> {
            let profile = self.agent_profile(metrics);
            let mut invocation = match role {
                WorkRole::Evidence => execution::observation::build_invocation(
                    profile,
                    request,
                    budget.max_input_bytes,
                    budget.max_result_bytes,
                ),
                WorkRole::Screening => execution::observation::build_screening_invocation(
                    profile,
                    request,
                    budget.max_result_bytes,
                ),
                WorkRole::Finalizer => execution::finalization::build_invocation(profile, request),
                WorkRole::Discovery => {
                    return Err(miette::miette!(
                        "discovery stages require the structured result adapter"
                    ));
                }
            }?;
            self.configure_child(&mut invocation.command);
            let limit = budget
                .max_result_bytes
                .checked_mul(3)
                .and_then(|limit| limit.checked_add(16384))
                .ok_or_else(|| miette::miette!("host output envelope limit overflow"))?;
            let workload = matches!(role, WorkRole::Evidence)
                .then(|| {
                    crate::commands::work::workload::ObservationWorkload::from_request(request)
                })
                .transpose()?;
            Ok((invocation, limit, workload))
        }

        async fn request_observation(
            &mut self,
            request: serde_json::Value,
            metrics: &mut WorkStepMetrics,
        ) -> Option<ObservationReply> {
            metrics.request_json_bytes = Some(
                serde_json::to_vec(&request)
                    .expect("JSON request serializes")
                    .len(),
            );
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return None;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                self.failure(
                    "bounded work receipt",
                    miette::miette!("semantic execution is disabled by llm.provider"),
                );
                return None;
            }
            let budget = match self.config.execution.effective_work() {
                Ok(budget) => budget,
                Err(error) => {
                    self.failure("semantic work budget", typed_error(error));
                    return None;
                }
            };
            if !self.call_budget_available() {
                return None;
            }
            if let Err(error) = self.prepare_agent(metrics).await {
                if let Some(unavailable) = error.downcast_ref::<HostUnavailable>() {
                    metrics.outcome = WorkOutcome::Unsent;
                    return Some(ObservationReply::HostUnavailable(unavailable.to_string()));
                }
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                    self.record(
                        "bounded work receipt",
                        StageState::Cancelled,
                        Some(error.to_string()),
                    )
                } else {
                    metrics.outcome = WorkOutcome::Failed;
                    self.failure("bounded work receipt", error)
                };
                return None;
            }
            let (invocation, limit, workload) =
                match self.observation_invocation(&request, metrics.role, metrics, budget) {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        self.failure("bounded work receipt", error);
                        return None;
                    }
                };
            metrics.request_json_bytes = Some(invocation.request_json_bytes);
            metrics.workload = workload;
            let timeout = self.agent_profile(metrics).timeout();
            if let Some(binding) = self
                .queue_turn
                .as_ref()
                .and_then(|turn| turn.binding.as_ref())
            {
                let actual = match metrics.role {
                    WorkRole::Evidence => format!(
                        "work:{}",
                        request["work_id"].as_str().expect("prepared work identity")
                    ),
                    WorkRole::Screening => format!(
                        "screening:{}",
                        metrics
                            .screening_id
                            .as_deref()
                            .expect("prepared screening identity")
                    ),
                    WorkRole::Finalizer => format!(
                        "work:{}",
                        request["work"]["work_id"]
                            .as_str()
                            .expect("prepared finalizer identity")
                    ),
                    WorkRole::Discovery => {
                        unreachable!("discovery stages have no observation request")
                    }
                };
                if binding.identity != actual {
                    self.queue_admitted.clear();
                    self.queue_replan = QueueReplan::Invalidate;
                    self.record(
                        "queue phase refresh",
                        StageState::Skipped,
                        Some(
                            "selected phase changed; preparing current work before invocation"
                                .into(),
                        ),
                    );
                    return None;
                }
            }
            if !self.reserve_turn().await {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return None;
            }
            let started_at = Instant::now();
            let outcome = self
                .supervisor
                .run_request(
                    invocation.command,
                    Some(timeout),
                    Some(limit),
                    Some(&invocation.prompt),
                    self.host_output,
                )
                .await;
            let duration_ms = started_at.elapsed().as_millis() as u64;
            metrics.agent_duration_ms = Some(duration_ms);
            self.result.semantic_duration_ms += duration_ms;
            let child = match outcome {
                Ok(child) => child,
                Err(error) => match self.unstarted_call(metrics, error) {
                    Ok(reason) => return Some(ObservationReply::HostUnavailable(reason)),
                    Err(error) => {
                        metrics.outcome = WorkOutcome::Failed;
                        self.failure("bounded work receipt", error);
                        return None;
                    }
                },
            };
            metrics.host_end = Self::host_end(metrics, child.finished_at);
            if !child.success {
                if child.termination == execution::ChildTermination::TimedOut
                    && self.configuration_is_current()
                {
                    metrics.outcome = WorkOutcome::Failed;
                    metrics.unanswered = true;
                    metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                        invocation.engine,
                        execution::diagnostic::Phase::Invocation,
                        &child,
                    ));
                    return Some(ObservationReply::InvocationFailed(
                        crate::commands::work::state::InvocationFailure::TimedOut {
                            engine: invocation.engine,
                            host: metrics.host_end.clone(),
                        },
                    ));
                }
                if let Some(code) = child.failed_exit()
                    && self.configuration_is_current()
                {
                    metrics.outcome = WorkOutcome::Failed;
                    metrics.unanswered = true;
                    metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                        invocation.engine,
                        execution::diagnostic::Phase::Invocation,
                        &child,
                    ));
                    return Some(ObservationReply::InvocationFailed(
                        crate::commands::work::state::InvocationFailure::Exited {
                            engine: invocation.engine,
                            code,
                            host: metrics.host_end.clone(),
                        },
                    ));
                }
                if child.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                    self.record("bounded work receipt", StageState::Cancelled, child.detail);
                } else {
                    metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                        invocation.engine,
                        execution::diagnostic::Phase::Invocation,
                        &child,
                    ));
                    metrics.outcome = WorkOutcome::Failed;
                    self.halt("bounded work receipt", child.detail);
                }
                self.configuration_is_current();
                return None;
            }
            if !self.configuration_is_current() {
                // The reply answered a configuration the run no longer holds.
                metrics.outcome = WorkOutcome::InputsChanged;
                return None;
            }
            Some(ObservationReply::Output {
                engine: invocation.engine,
                child,
                limit: budget.max_result_bytes,
            })
        }

        async fn observe(
            &mut self,
            id: &str,
            node: &str,
            request: serde_json::Value,
            completed: usize,
            metrics: &mut WorkStepMetrics,
        ) -> ObservationStep {
            let Some(reply) = self.request_observation(request.clone(), metrics).await else {
                return ObservationStep::Failed;
            };
            if let ObservationReply::HostUnavailable(reason) = reply {
                self.inspection_deferred.insert(id.into(), reason.clone());
                self.queue_admitted.clear();
                self.deferred("host unavailable", format!("{node}: {reason}"));
                return ObservationStep::Deferred(reason);
            }
            self.record_observation(id, node, &request, completed, reply, metrics)
                .await
        }

        fn decode_observation_reply(
            &mut self,
            request: &serde_json::Value,
            reply: ObservationReply,
            metrics: &mut WorkStepMetrics,
        ) -> Option<crate::commands::work::InspectionResponse> {
            let (engine, child, limit) = match reply {
                ObservationReply::Output {
                    engine,
                    child,
                    limit,
                } => (engine, child, limit),
                ObservationReply::InvocationFailed(failure) => {
                    return Some(crate::commands::work::InspectionResponse::InvocationFailed(
                        failure,
                    ));
                }
                ObservationReply::HostUnavailable(_) => {
                    unreachable!("an unsent observation is deferred before it is decoded")
                }
            };
            let bytes = &child.stdout;
            let answerless = execution::observation::is_answerless(engine, bytes);
            metrics.unanswered |= answerless;
            metrics.answered = !answerless;
            let host = metrics.host_end.clone();
            let invalid = || {
                crate::commands::work::InspectionResponse::InvocationFailed(
                    execution::observation::response_failure(engine, bytes, host.clone()),
                )
            };
            let output = match execution::observation::decode_response(engine, bytes, limit) {
                Ok(output) => output,
                Err(error) => {
                    if let Some(capacity) =
                        error.downcast_ref::<crate::commands::work::state::OutputCapacity>()
                    {
                        metrics.result_json_bytes = Some(capacity.actual_bytes);
                        if execution::observation::validate_evidence_response(engine, bytes)
                            .is_err()
                        {
                            return Some(invalid());
                        }
                        return Some(crate::commands::work::InspectionResponse::Capacity(
                            capacity.clone(),
                        ));
                    }
                    metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                        engine,
                        execution::diagnostic::Phase::Response,
                        &child,
                    ));
                    return Some(invalid());
                }
            };
            metrics.result_json_bytes = Some(
                serde_json::to_vec(&output)
                    .expect("decoded observation serializes")
                    .len(),
            );
            let decoded = execution::observation::decode_observation(output, request, limit);
            if (decoded.is_ok()
                || decoded.as_ref().err().is_some_and(|error| {
                    error
                        .downcast_ref::<crate::commands::work::state::OutputCapacity>()
                        .is_some()
                }))
                && execution::observation::validate_evidence_response(engine, bytes).is_err()
            {
                return Some(invalid());
            }
            let candidate = |output, text_violation| {
                Some(crate::commands::work::InspectionResponse::Complete(
                    Box::new(crate::commands::work::state::ObservationCandidate {
                        output,
                        text_violation,
                        response_revision: lk_core::rss_original::digest(bytes),
                        response_json_bytes: metrics
                            .result_json_bytes
                            .expect("decoded observation size"),
                    }),
                ))
            };
            match decoded {
                Ok(execution::observation::ObservationResponse::Complete(output)) => {
                    candidate(output, None)
                }
                Ok(execution::observation::ObservationResponse::InvalidText {
                    output,
                    violation,
                }) => candidate(output, Some(violation)),
                Ok(execution::observation::ObservationResponse::NeedsAction(reason)) => Some(
                    crate::commands::work::InspectionResponse::NeedsAction(reason),
                ),
                Ok(execution::observation::ObservationResponse::SmallerInspection(reason)) => Some(
                    crate::commands::work::InspectionResponse::SmallerInspection {
                        reason,
                        response: crate::commands::work::state::RefinementResponse {
                            response_revision: lk_core::rss_original::digest(bytes),
                            response_json_bytes: metrics
                                .result_json_bytes
                                .expect("decoded response size"),
                        },
                    },
                ),
                Err(error) => {
                    if let Some(capacity) =
                        error.downcast_ref::<crate::commands::work::state::OutputCapacity>()
                    {
                        return Some(crate::commands::work::InspectionResponse::Capacity(
                            capacity.clone(),
                        ));
                    }
                    Some(invalid())
                }
            }
        }

        fn track_inspection_recording(
            &mut self,
            recorded: miette::Result<crate::commands::work::RecordedInspections>,
            total_duration_ms: u64,
        ) -> (
            miette::Result<crate::commands::work::RecordedInspections>,
            u64,
        ) {
            match recorded {
                Ok(recorded) => {
                    let record_ms = recorded
                        .records
                        .iter()
                        .map(|record| record.duration_ms)
                        .sum::<u64>();
                    self.result.work_telemetry.evidence_revalidation_duration_ms +=
                        recorded.revalidation_duration_ms;
                    self.result.work_telemetry.evidence_record_duration_ms += record_ms;
                    let shared_ms = total_duration_ms.saturating_sub(record_ms);
                    (Ok(recorded), shared_ms)
                }
                Err(error) => (Err(error), total_duration_ms),
            }
        }

        async fn record_observation_batch(
            &mut self,
            replies: Vec<crate::commands::work::InspectionReply>,
        ) -> Option<(
            miette::Result<crate::commands::work::RecordedInspections>,
            u64,
        )> {
            let started = Instant::now();
            let (session, _) = self.recording_session("bounded work receipt").await?;
            let recorded = crate::commands::work::record_inspections(
                &session,
                &self.config,
                &self.config_revision,
                replies,
                self.supervisor.read_control(),
            );
            Some(self.track_inspection_recording(recorded, started.elapsed().as_millis() as u64))
        }

        fn finish_observation_record(
            &mut self,
            id: &str,
            node: &str,
            completed: usize,
            result: miette::Result<crate::commands::work::InspectionRecordOutcome>,
            metrics: &mut WorkStepMetrics,
        ) -> ObservationStep {
            use crate::commands::work::InspectionRecordOutcome;
            match result {
                Ok(InspectionRecordOutcome::InvocationFailed(reason)) => {
                    metrics.outcome = WorkOutcome::Failed;
                    self.inspection_deferred.insert(id.into(), reason.clone());
                    self.queue_admitted.clear();
                    self.deferred("bounded invocation held", format!("{node}: {reason}"));
                    ObservationStep::InvocationFailed
                }
                Ok(InspectionRecordOutcome::Refined { added_nodes }) => {
                    metrics.outcome = WorkOutcome::Refined;
                    self.result.work_telemetry.retained_progress.action_receipts += 1;
                    self.queue_replan = QueueReplan::Invalidate;
                    self.record("bounded inspection refinement", StageState::Completed,
                        Some(format!("{node}: retained native refinement request and added {added_nodes} observation nodes; no observation recorded")));
                    ObservationStep::Refined
                }
                Ok(InspectionRecordOutcome::Recorded(progress)) => {
                    if progress.completed <= completed
                        || progress.ready.iter().any(|ready| ready == node)
                    {
                        self.failure("bounded work receipt", miette::miette!("worker exited without completing its selected node; unfinished work retained"));
                        return ObservationStep::Failed;
                    }
                    metrics.outcome = WorkOutcome::Recorded;
                    self.record(
                        "bounded work receipt",
                        StageState::Completed,
                        Some(node.into()),
                    );
                    ObservationStep::Recorded
                }
                Ok(outcome) => {
                    let (stage, reason) = match outcome {
                        InspectionRecordOutcome::Deferred(reason) => {
                            metrics.outcome = WorkOutcome::NeedsAction;
                            ("bounded work action", reason)
                        }
                        InspectionRecordOutcome::Rejected(reason) => {
                            metrics.outcome = WorkOutcome::Rejected;
                            ("bounded output capacity", reason)
                        }
                        InspectionRecordOutcome::InvalidText(reason) => {
                            metrics.outcome = WorkOutcome::Rejected;
                            ("bounded observation text", reason)
                        }
                        InspectionRecordOutcome::InvalidEvidence(reason) => {
                            metrics.outcome = WorkOutcome::Rejected;
                            ("bounded observation evidence", reason)
                        }
                        InspectionRecordOutcome::Recorded(_)
                        | InspectionRecordOutcome::InvocationFailed(_)
                        | InspectionRecordOutcome::Refined { .. } => unreachable!(),
                    };
                    self.result.work_telemetry.retained_progress.action_receipts += 1;
                    self.inspection_deferred.insert(id.into(), reason.clone());
                    self.deferred(stage, &reason);
                    ObservationStep::Deferred(reason)
                }
                Err(error) if is_work_input_changed(&error) => {
                    metrics.outcome = WorkOutcome::InputsChanged;
                    ObservationStep::InputsChanged(error)
                }
                Err(error) => {
                    self.failure("bounded work receipt", error);
                    ObservationStep::Failed
                }
            }
        }

        async fn record_observation(
            &mut self,
            id: &str,
            node: &str,
            request: &serde_json::Value,
            completed: usize,
            reply: ObservationReply,
            metrics: &mut WorkStepMetrics,
        ) -> ObservationStep {
            let Some(response) = self.decode_observation_reply(request, reply, metrics) else {
                return ObservationStep::Failed;
            };
            let replies = vec![crate::commands::work::InspectionReply {
                work_id: id.into(),
                node_id: node.into(),
                request: request.clone(),
                response,
            }];
            let Some((recorded, shared_ms)) = self.record_observation_batch(replies).await else {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return ObservationStep::Failed;
            };
            let mut recorded = match recorded {
                Ok(recorded) => recorded,
                Err(error) => {
                    metrics.revalidation_record_duration_ms = Some(shared_ms);
                    self.failure("bounded work receipt", error);
                    return ObservationStep::Failed;
                }
            };
            let record = recorded.records.pop().expect("one selected observation");
            metrics.revalidation_record_duration_ms = Some(shared_ms + record.duration_ms);
            self.finish_observation_record(id, node, completed, record.result, metrics)
        }

        fn yield_to_waiter(&mut self) -> bool {
            if self.result.yield_reason.is_some() {
                return true;
            }
            if self.automation.is_none()
                || matches!(self.result.operation, Operation::Daily)
                || self.result.is_halted()
                || self.result.cancelled
                || !self.result.work_telemetry.retained_progress.has_progress()
            {
                return false;
            }
            if !self.configuration_is_current() {
                return true;
            }
            let control = self.automation.as_ref().expect("scheduled checkpoint");
            if !control.has_remaining_budget() || !control.may_yield() {
                return false;
            }
            let Some(lease) = &self.lease else {
                self.failure(
                    "workflow checkpoint",
                    miette::miette!("yield requires an owned workflow"),
                );
                return true;
            };
            match lease.admission_contended() {
                Ok(false) => false,
                Ok(true) => {
                    self.result.yield_reason = Some(YieldReason::WorkflowContention);
                    self.record("workflow checkpoint", StageState::Skipped, Some("paid results retained; yielding to a waiting workflow before fresh preparation".into()));
                    true
                }
                Err(error) => {
                    self.failure("workflow checkpoint", error);
                    true
                }
            }
        }

        fn call_budget_available(&mut self) -> bool {
            self.call_limit_available() && !self.yield_to_waiter()
        }

        fn call_limit_available(&mut self) -> bool {
            if let Some(control) = &mut self.automation {
                if !control.check_duration() {
                    return self.record_automation_stop();
                }
                if control.available() == 0 {
                    control.stop_for_limit();
                    return self.record_automation_stop();
                }
            }
            match self.config.execution.effective_work() {
                Ok(budget) if self.result.semantic_calls < budget.max_calls => true,
                Ok(_) => {
                    self.result.budget_exhausted = true;
                    self.deferred("semantic call budget", "configured execution.work.max_calls exhausted; unfinished work is retained for the next run")
                }
                Err(error) => self.failure("semantic work budget", typed_error(error)),
            }
        }

        fn remaining_calls(&self, budget: lk_core::execution::SemanticWorkConfig) -> usize {
            let remaining = budget.max_calls.saturating_sub(self.result.semantic_calls);
            self.automation
                .as_ref()
                .map_or(remaining, |control| remaining.min(control.available()))
        }

        fn record_automation_stop(&mut self) -> bool {
            let report = self
                .automation
                .as_ref()
                .expect("scheduled run")
                .report
                .clone();
            let already_reported = self
                .result
                .automation
                .as_ref()
                .is_some_and(|before| before.stop == report.stop);
            let stop = report.stop.expect("automation stop");
            self.result.automation = Some(report);
            if !already_reported {
                self.deferred(
                    "scheduled invocation",
                    format!("{stop:?}; unfinished work is retained; inspect the automation report"),
                );
            }
            false
        }

        /// Whether `count` calls would be admitted now: the configuration, call limit,
        /// duration, cancellation and automation allowance checks `reserve_calls` makes,
        /// without recording a reservation: a cheap check before waiting for the scheduling
        /// claim. The calls are reserved under that claim before any cursor is written.
        fn admits_calls(&mut self, count: usize) -> bool {
            if !self.configuration_is_current() || !self.call_limit_available() {
                return false;
            }
            if self.supervisor.is_cancelled() {
                return self.record(
                    "agent admission",
                    StageState::Cancelled,
                    Some("cancelled before invocation".into()),
                );
            }
            if let Some(control) = &mut self.automation
                && control.available() < count
            {
                control.stop_for_limit();
                return self.record_automation_stop();
            }
            true
        }

        fn reserve_calls(&mut self, count: usize) -> bool {
            if !self.configuration_is_current() || !self.call_limit_available() {
                return false;
            }
            if self.supervisor.is_cancelled() {
                return self.record(
                    "agent admission",
                    StageState::Cancelled,
                    Some("cancelled before invocation".into()),
                );
            }
            if let Some(control) = &mut self.automation {
                let Some(lease) = &self.lease else {
                    return self.failure(
                        "automation admission",
                        miette::miette!("an owned vault workflow is required"),
                    );
                };
                match control.reserve(lease.root_pin().root(), count) {
                    Ok(true) => {}
                    Ok(false) => return self.record_automation_stop(),
                    Err(error) => return self.failure("automation reservation", error),
                }
            }
            if !self.configuration_is_current() {
                return false;
            }
            if self.supervisor.is_cancelled() {
                return self.record(
                    "agent admission",
                    StageState::Cancelled,
                    Some("cancelled after reservation".into()),
                );
            }
            self.result.semantic_calls += count;
            true
        }

        /// Discards queue planning captures after the controller waited for another writer:
        /// that writer may have changed the inputs they were derived from.
        fn refresh_queue_inputs(&mut self) {
            self.queue_admitted.clear();
            self.queue_replan = QueueReplan::Invalidate;
            self.record(
                "queue input refresh",
                StageState::Skipped,
                Some(
                    "writer access became available; preparing current inputs before invocation"
                        .into(),
                ),
            );
        }

        /// Reserves the pending single turn's call and then its place, a queue turn or an audit
        /// subject. The call is reserved under the scheduling claim, after any wait for it, and
        /// the cursor is written only once the call was reserved, so a turn the budget,
        /// duration or cancellation refuses keeps its place.
        async fn reserve_turn(&mut self) -> bool {
            if !self.admits_calls(1) {
                return false;
            }
            if let Some((keys, subject)) = self.audit_turn.take() {
                let Some((session, _)) = self.mutation_session("audit scheduling").await else {
                    return false;
                };
                if !self.reserve_calls(1) {
                    return false;
                }
                if let Err(error) = scheduler::reserve_in_session(
                    &session,
                    scheduler::Stage::Audit,
                    &keys,
                    std::slice::from_ref(&subject),
                ) {
                    return self.failure("audit scheduling", error);
                }
                return true;
            }
            let Some(turn) = self.queue_turn.take() else {
                return self.reserve_calls(1);
            };
            self.reserved_targets = turn.targets.clone();
            let Some((session, waited)) = self.mutation_session("queue scheduling").await else {
                return false;
            };
            if waited {
                self.refresh_queue_inputs();
                return false;
            }
            if !self.reserve_calls(1) {
                return false;
            }
            let continuation = turn.binding.as_ref().is_some_and(|binding| {
                self.queue_admitted
                    .can_continue(std::slice::from_ref(binding))
            });
            if !continuation {
                if let Err(error) = scheduler::reserve_in_session(
                    &session,
                    scheduler::Stage::Queue,
                    &turn.keys,
                    &turn.targets,
                ) {
                    return self.failure("queue scheduling", error);
                }
                self.queue_admitted.clear();
                if let Some(binding) = &turn.binding
                    && let Err(error) = self.queue_admitted.admit(std::slice::from_ref(binding))
                {
                    return self.failure("queue scheduling", error);
                }
            }
            if let Some(binding) = &turn.binding
                && let Err(error) = self.queue_admitted.charge(std::slice::from_ref(binding))
            {
                return self.failure("queue scheduling", error);
            }
            true
        }

        async fn bounded_audit_wave(
            &mut self,
            selected: &str,
            jobs: &std::collections::BTreeMap<String, String>,
            keys: &std::collections::BTreeSet<String>,
        ) -> Option<bool> {
            let budget = match self.config.execution.effective_work() {
                Ok(budget) => budget,
                Err(error) => {
                    return Some(self.failure("semantic work budget", typed_error(error)));
                }
            };
            let limit = self.wave_limit(budget);
            if limit <= 1 || !jobs.contains_key(selected) {
                return None;
            }
            let mut order = keys.iter().cloned().collect::<Vec<_>>();
            let first = order
                .iter()
                .position(|target| target == selected)
                .expect("selected queue target");
            order.rotate_left(first);
            let mut prepared = std::collections::BTreeMap::<String, PreparedObservations>::new();
            let mut request_counts = std::collections::BTreeMap::<String, usize>::new();
            let mut planned_targets = Vec::new();
            let mut index = 0;
            while planned_targets.len() < limit {
                let target = &order[index % order.len()];
                index += 1;
                let Some(id) = jobs.get(target) else {
                    break;
                };
                *request_counts.entry(id.clone()).or_default() += 1;
                planned_targets.push(target.clone());
            }
            let demands = request_counts.into_iter().collect::<Vec<_>>();
            // Audit Work is revalidated against current inputs here, so a wait needs no replan.
            let Some((session, _)) = self.mutation_session("bounded work progress").await else {
                return Some(false);
            };
            let captured = match crate::commands::work::prepare_inspections_batch(
                &session,
                &self.config,
                &self.config_revision,
                &demands,
                self.supervisor.read_control(),
            ) {
                Ok(captured) => captured,
                Err(error) if is_semantic_context_changed(&error) => return None,
                Err(error) => return Some(self.failure("bounded work progress", error)),
            };
            self.result.work_telemetry.evidence_preparation_duration_ms += captured.duration_ms;
            let mut preparation_duration_ms = Some(captured.duration_ms);
            for ((id, _), result) in demands.into_iter().zip(captured.turns) {
                let (progress, requests) = match result {
                    Ok(captured) => captured,
                    Err(error) if crate::commands::work::state::is_input_capacity(&error) => {
                        return None;
                    }
                    Err(error) if is_semantic_context_changed(&error) => return None,
                    Err(error) => return Some(self.failure("bounded work progress", error)),
                };
                if !progress.complete && requests.is_empty() {
                    return Some(self.failure(
                        "bounded work progress",
                        miette::miette!("incomplete work has no ready node"),
                    ));
                }
                prepared.insert(
                    id,
                    PreparedObservations {
                        completed: progress.completed,
                        requests: requests.into(),
                    },
                );
            }
            let mut turns = Vec::new();
            let mut reserved_targets = Vec::new();
            for target in planned_targets {
                let Some(id) = jobs.get(&target) else { break };
                let work = prepared.get_mut(id).expect("captured work");
                let Some(request) = work.requests.pop_front() else {
                    break;
                };
                match crate::commands::work::inspection_action(session.root(), id, &request) {
                    Ok(Some(reason)) => {
                        self.inspection_deferred.insert(id.clone(), reason.clone());
                        self.deferred("bounded work action", reason);
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => return Some(self.failure("bounded work action", error)),
                }
                reserved_targets.push(target.clone());
                let node_id = request["node_id"]
                    .as_str()
                    .expect("prepared node id")
                    .to_owned();
                let mut metrics = WorkStepMetrics::new(WorkRole::Evidence);
                metrics.work_id = Some(id.clone());
                metrics.node_id = Some(node_id.clone());
                metrics.node_kind = match request["kind"].as_str() {
                    Some("inspect") => Some(NodeKind::Inspect),
                    Some("reduce") => Some(NodeKind::Reduce),
                    _ => unreachable!("prepared evidence node kind"),
                };
                metrics.preparation_duration_ms = preparation_duration_ms.take();
                metrics.request_json_bytes = Some(
                    serde_json::to_vec(&request)
                        .expect("JSON request serializes")
                        .len(),
                );
                turns.push(ObservationTurn {
                    target: None,
                    audit_subject: Some(target.clone()),
                    work_id: id.clone(),
                    node_id,
                    completed: work.completed,
                    request,
                    metrics,
                });
            }
            drop(session);
            if turns.is_empty() {
                return if self.inspection_deferred.contains_key(&jobs[selected]) {
                    Some(true)
                } else {
                    None
                };
            }
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return Some(false);
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                return Some(self.failure(
                    "bounded work receipt",
                    miette::miette!("semantic execution is disabled by llm.provider"),
                ));
            }
            let mut requests = Vec::new();
            let mut directories = Vec::new();
            let mut engines = Vec::new();
            // A turn routed to a host this run tripped is deferred alone, and the wave still
            // sends the turns of healthy hosts.
            let mut ready = Vec::new();
            let mut sent_subjects = Vec::new();
            let mut pending = turns.into_iter().zip(reserved_targets.iter());
            while let Some((mut turn, subject)) = pending.next() {
                if let Err(error) = self.prepare_agent(&mut turn.metrics).await {
                    if let Some(unavailable) = error.downcast_ref::<HostUnavailable>() {
                        let reason = unavailable.to_string();
                        self.inspection_deferred
                            .insert(turn.work_id.clone(), reason.clone());
                        self.deferred("host unavailable", format!("{subject}: {reason}"));
                        turn.metrics.outcome = WorkOutcome::Unsent;
                        self.record_work_step(turn.metrics);
                        continue;
                    }
                    for mut turn in ready
                        .into_iter()
                        .chain([turn])
                        .chain(pending.map(|(turn, _)| turn))
                    {
                        turn.metrics.outcome = if self.result.cancelled {
                            WorkOutcome::Cancelled
                        } else {
                            WorkOutcome::Failed
                        };
                        self.record_work_step(turn.metrics);
                    }
                    return Some(self.agent_preflight_failure("bounded work receipt", error));
                }
                let timeout = self.agent_profile(&turn.metrics).timeout();
                let (invocation, capture_limit, workload) = match self.observation_invocation(
                    &turn.request,
                    WorkRole::Evidence,
                    &turn.metrics,
                    budget,
                ) {
                    Ok(invocation) => invocation,
                    Err(error) => return Some(self.failure("bounded work receipt", error)),
                };
                turn.metrics.request_json_bytes = Some(invocation.request_json_bytes);
                turn.metrics.workload = workload;
                engines.push(invocation.engine);
                let (request, directory) = invocation.into_request(timeout, capture_limit);
                requests.push(request);
                directories.push(directory);
                ready.push(turn);
                sent_subjects.push(subject.clone());
            }
            let turns = ready;
            if turns.is_empty() {
                return Some(true);
            }
            // The cursor passes only subjects this wave sends: a subject none of whose turns
            // was sent leaves the eligible set, and the reservation is checked against the rest.
            let unsent = reserved_targets
                .iter()
                .filter(|subject| !sent_subjects.contains(subject))
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let eligible = keys
                .difference(&unsent)
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            let keys = &eligible;
            let reservations = reserved_targets
                .iter()
                .filter(|subject| !unsent.contains(*subject))
                .cloned()
                .collect::<Vec<_>>();
            let reservations = reservations.as_slice();
            if !self.admits_calls(requests.len()) {
                self.record_uninvoked_observations(turns);
                return Some(false);
            }
            {
                let Some((session, waited)) =
                    self.mutation_session("bounded work scheduling").await
                else {
                    self.record_uninvoked_observations(turns);
                    return Some(false);
                };
                if waited {
                    for turn in &turns {
                        if let Err(error) = crate::commands::work::validate_inspection_request(
                            session.root(),
                            &self.config,
                            &self.config_revision,
                            &turn.work_id,
                            &turn.node_id,
                            &turn.request,
                            self.supervisor.read_control(),
                        ) {
                            if is_semantic_context_changed(&error) {
                                self.record_uninvoked_observations(turns);
                                return None;
                            }
                            return Some(self.failure("bounded work scheduling", error));
                        }
                    }
                }
                // The calls are reserved before the cursor passes their subjects, so a wave
                // refused after waiting for the claim keeps its place.
                if !self.reserve_calls(requests.len()) {
                    self.record_uninvoked_observations(turns);
                    return Some(false);
                }
                if let Err(error) = scheduler::reserve_in_session(
                    &session,
                    scheduler::Stage::Audit,
                    keys,
                    reservations,
                ) {
                    return Some(self.failure("bounded work scheduling", error));
                }
            }
            let attempts = self
                .supervisor
                .run_requests(requests, self.host_output)
                .await;
            drop(directories);
            let cancelled = attempts
                .iter()
                .any(|attempt| attempt.result.as_ref().is_ok_and(|child| child.cancelled));
            Some(
                self.settle_observation_attempts(
                    turns,
                    attempts,
                    engines,
                    budget.max_result_bytes,
                    cancelled,
                )
                .await,
            )
        }

        fn decode_observation_attempts(
            &mut self,
            turns: Vec<ObservationTurn>,
            attempts: Vec<execution::ChildAttempt>,
            engines: Vec<lk_core::execution::AgentEngine>,
            result_limit: usize,
            cancelled: bool,
        ) -> DecodedObservations {
            let current = self.configuration_is_current();
            if cancelled {
                self.record(
                    "bounded work receipt",
                    StageState::Cancelled,
                    Some(
                        "parallel evidence inspection interrupted; unfinished work retained".into(),
                    ),
                );
            }
            let mut accepted = current && !cancelled;
            let mut replies = Vec::new();
            let mut returned = Vec::new();
            // Turns whose host could not start: deferred with their profile, never fatal.
            let mut unstarted = false;
            for ((mut turn, attempt), engine) in turns.into_iter().zip(attempts).zip(engines) {
                turn.metrics.agent_duration_ms = Some(attempt.duration_ms);
                turn.metrics.host_end = match &attempt.result {
                    Ok(child) => Self::host_end(&turn.metrics, child.finished_at),
                    Err(_) => None,
                };
                self.result.semantic_duration_ms += attempt.duration_ms;
                if cancelled || !current {
                    turn.metrics.outcome = if cancelled {
                        WorkOutcome::Cancelled
                    } else {
                        WorkOutcome::InputsChanged
                    };
                    self.record_work_step(turn.metrics);
                    continue;
                }
                let child = match attempt.result {
                    Ok(child) if child.success => child,
                    Ok(child) if child.termination == execution::ChildTermination::TimedOut => {
                        turn.metrics.unanswered = true;
                        turn.metrics.host_failure =
                            Some(execution::diagnostic::HostFailure::from_child(
                                engine,
                                execution::diagnostic::Phase::Invocation,
                                &child,
                            ));
                        turn.metrics.outcome = WorkOutcome::Failed;
                        replies.push(crate::commands::work::InspectionReply {
                            work_id: turn.work_id.clone(),
                            node_id: turn.node_id.clone(),
                            request: turn.request.clone(),
                            response: crate::commands::work::InspectionResponse::InvocationFailed(
                                crate::commands::work::state::InvocationFailure::TimedOut {
                                    engine,
                                    host: turn.metrics.host_end.clone(),
                                },
                            ),
                        });
                        returned.push(turn);
                        continue;
                    }
                    Ok(child) if child.failed_exit().is_some() => {
                        turn.metrics.unanswered = true;
                        turn.metrics.host_failure =
                            Some(execution::diagnostic::HostFailure::from_child(
                                engine,
                                execution::diagnostic::Phase::Invocation,
                                &child,
                            ));
                        turn.metrics.outcome = WorkOutcome::Failed;
                        replies.push(crate::commands::work::InspectionReply {
                            work_id: turn.work_id.clone(),
                            node_id: turn.node_id.clone(),
                            request: turn.request.clone(),
                            response: crate::commands::work::InspectionResponse::InvocationFailed(
                                crate::commands::work::state::InvocationFailure::Exited {
                                    engine,
                                    code: child.failed_exit().expect("numeric failed exit"),
                                    host: turn.metrics.host_end.clone(),
                                },
                            ),
                        });
                        returned.push(turn);
                        continue;
                    }
                    Ok(child) => {
                        accepted = false;
                        turn.metrics.host_failure =
                            Some(execution::diagnostic::HostFailure::from_child(
                                engine,
                                execution::diagnostic::Phase::Invocation,
                                &child,
                            ));
                        turn.metrics.outcome = WorkOutcome::Failed;
                        self.halt("bounded work receipt", child.detail);
                        self.record_work_step(turn.metrics);
                        continue;
                    }
                    Err(error) => match self.unstarted_call(&mut turn.metrics, error) {
                        Ok(reason) => {
                            self.inspection_deferred
                                .insert(turn.work_id.clone(), reason.clone());
                            self.deferred(
                                "host unavailable",
                                format!("{}: {reason}", turn.work_id),
                            );
                            unstarted = true;
                            self.record_work_step(turn.metrics);
                            continue;
                        }
                        Err(error) => {
                            accepted = false;
                            turn.metrics.outcome = WorkOutcome::Failed;
                            self.failure("bounded work receipt", error);
                            self.record_work_step(turn.metrics);
                            continue;
                        }
                    },
                };
                if !self.configuration_is_current() {
                    accepted = false;
                    self.record_work_step(turn.metrics);
                    continue;
                }
                let Some(response) = self.decode_observation_reply(
                    &turn.request,
                    ObservationReply::Output {
                        engine,
                        child,
                        limit: result_limit,
                    },
                    &mut turn.metrics,
                ) else {
                    accepted = false;
                    self.record_work_step(turn.metrics);
                    continue;
                };
                replies.push(crate::commands::work::InspectionReply {
                    work_id: turn.work_id.clone(),
                    node_id: turn.node_id.clone(),
                    request: turn.request.clone(),
                    response,
                });
                returned.push(turn);
            }
            DecodedObservations {
                accepted,
                replies,
                returned,
                unstarted,
            }
        }
        fn finish_observation_attempts(
            &mut self,
            decoded: DecodedObservations,
            recorded: Option<(
                miette::Result<crate::commands::work::RecordedInspections>,
                u64,
            )>,
        ) -> bool {
            let DecodedObservations {
                mut accepted,
                replies: _,
                mut returned,
                unstarted,
            } = decoded;
            let mut usable = false;
            let mut failed = false;
            let mut replanned = false;
            // Replies to one Work share its fresh validation, which is charged once.
            let mut charged = std::collections::BTreeSet::new();
            if !returned.is_empty() {
                match recorded {
                    Some((Ok(recorded), shared_ms)) => {
                        for (index, (mut turn, record)) in
                            returned.into_iter().zip(recorded.records).enumerate()
                        {
                            turn.metrics.revalidation_record_duration_ms =
                                Some(record.duration_ms + if index == 0 { shared_ms } else { 0 });
                            let step = self.finish_observation_record(
                                &turn.work_id,
                                &turn.node_id,
                                turn.completed,
                                record.result,
                                &mut turn.metrics,
                            );
                            if let ObservationStep::InputsChanged(error) = step {
                                self.record(
                                    "bounded work context",
                                    StageState::Skipped,
                                    Some(error.to_string()),
                                );
                                if charged.insert(turn.work_id.clone()) {
                                    if let Some(target) = &turn.target
                                        && !self
                                            .finalization_replans
                                            .try_spend(target, FINALIZATION_CHANGED_AGAIN)
                                    {
                                        self.deferred("finalization inputs unstable", target);
                                    }
                                    if let Some(subject) = &turn.audit_subject {
                                        self.audit_wave_replans.insert(subject.clone());
                                    }
                                }
                                self.queue_replan = QueueReplan::Invalidate;
                                self.queue_admitted.clear();
                                replanned = true;
                            } else {
                                if matches!(step, ObservationStep::Failed) {
                                    accepted = false;
                                }
                                usable |= !matches!(
                                    step,
                                    ObservationStep::InvocationFailed | ObservationStep::Failed
                                );
                                failed |= matches!(step, ObservationStep::InvocationFailed);
                            }
                            self.record_work_step(turn.metrics);
                        }
                    }
                    Some((Err(error), duration_ms)) => {
                        accepted = false;
                        if let Some(first) = returned.first_mut() {
                            first.metrics.revalidation_record_duration_ms = Some(duration_ms);
                        }
                        self.failure("bounded work receipt", error);
                        self.record_uninvoked_observations(returned);
                    }
                    None => {
                        accepted = false;
                        self.record_uninvoked_observations(returned);
                    }
                }
            }
            if failed {
                self.replan_after_hold();
            }
            if unstarted {
                self.replan_after_hold();
            }
            accepted && (usable || replanned || failed || unstarted)
        }
        async fn settle_observation_attempts(
            &mut self,
            turns: Vec<ObservationTurn>,
            attempts: Vec<execution::ChildAttempt>,
            engines: Vec<lk_core::execution::AgentEngine>,
            result_limit: usize,
            cancelled: bool,
        ) -> bool {
            let mut decoded =
                self.decode_observation_attempts(turns, attempts, engines, result_limit, cancelled);
            let replies = std::mem::take(&mut decoded.replies);
            let recorded = if replies.is_empty() {
                None
            } else {
                self.record_observation_batch(replies).await
            };
            self.finish_observation_attempts(decoded, recorded)
        }

        fn record_uninvoked_observations(&mut self, turns: Vec<ObservationTurn>) {
            for mut turn in turns {
                if self.result.cancelled {
                    turn.metrics.outcome = WorkOutcome::Cancelled;
                }
                self.record_work_step(turn.metrics);
            }
        }

        async fn bounded_observation_step(
            &mut self,
            id: &str,
            before: crate::commands::work::state::WorkProgress,
            request: Option<serde_json::Value>,
            preparation_duration_ms: u64,
        ) -> ObservationStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Evidence);
            metrics.work_id = Some(id.into());
            metrics.preparation_duration_ms = Some(preparation_duration_ms);
            let Some(node) = before.ready.first() else {
                self.record_work_step(metrics);
                self.failure(
                    "bounded work progress",
                    miette::miette!("incomplete work has no ready node"),
                );
                return ObservationStep::Failed;
            };
            metrics.node_id = Some(node.clone());
            let Some(request) = request else {
                self.record_work_step(metrics);
                self.failure(
                    "bounded work progress",
                    miette::miette!("ready work has no inspection input"),
                );
                return ObservationStep::Failed;
            };
            metrics.node_kind = match request["kind"].as_str() {
                Some("inspect") => Some(NodeKind::Inspect),
                Some("reduce") => Some(NodeKind::Reduce),
                _ => None,
            };
            let result = match crate::commands::work::inspection_action(&self.root, id, &request) {
                Ok(Some(reason)) => {
                    self.inspection_deferred.insert(id.into(), reason.clone());
                    self.deferred("bounded work action", &reason);
                    ObservationStep::Deferred(reason)
                }
                Ok(None) => {
                    self.observe(id, node, request, before.completed, &mut metrics)
                        .await
                }
                Err(error) => {
                    self.failure("bounded work action", error);
                    ObservationStep::Failed
                }
            };
            self.record_work_step(metrics);
            result
        }

        /// Confirmed timeouts and numeric exits return `InvocationFailed` for the caller to hold
        /// its exact request; unusable remote replies do so only for read-only roles. Every other
        /// host failure halts.
        async fn request_result(
            &mut self,
            request: &serde_json::Value,
            schema: &serde_json::Value,
            metrics: &mut WorkStepMetrics,
        ) -> Option<ResultReply> {
            metrics.request_json_bytes = Some(
                serde_json::to_vec(request)
                    .expect("request serializes")
                    .len(),
            );
            if self.result.is_halted()
                || self.result.cancelled
                || !self.configuration_is_current()
                || !self.call_budget_available()
            {
                return None;
            }
            let budget = match self.config.execution.effective_work() {
                Ok(budget) => budget,
                Err(error) => {
                    self.failure("native result budget", typed_error(error));
                    return None;
                }
            };
            if metrics.request_json_bytes.expect("measured request") > budget.max_input_bytes {
                self.failure(
                    "native result request",
                    miette::miette!("complete request exceeds max_input_bytes"),
                );
                return None;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                self.failure(
                    "native result",
                    miette::miette!("semantic execution is disabled by llm.provider"),
                );
                return None;
            }
            metrics.channel = HostChannel::Result;
            if let Err(error) = self.prepare_agent(metrics).await {
                if let Some(unavailable) = error.downcast_ref::<HostUnavailable>() {
                    metrics.outcome = WorkOutcome::Unsent;
                    return Some(ResultReply::HostUnavailable(unavailable.to_string()));
                }
                self.agent_preflight_failure("native result host", error);
                return None;
            }
            let profile = self.agent_profile(metrics);
            let timeout = profile.timeout();
            let access = if matches!(metrics.role, WorkRole::Discovery | WorkRole::Screening) {
                execution::AgentAccess::ReadOnly
            } else {
                execution::AgentAccess::ReadWrite
            };
            let invocation = execution::result::build(
                profile,
                if metrics.stages == [ExecutionStage::Audit] {
                    "lore-wiki"
                } else {
                    "lore-process"
                },
                self.installation,
                self.opts.config.as_ref().expect("resolved config"),
                request,
                execution::result::Contract {
                    schema,
                    max_result_bytes: budget.max_result_bytes,
                    access,
                },
            );
            let execution::result::Invocation { mut agent, output } = match invocation {
                Ok(invocation) => invocation,
                Err(error) => {
                    self.failure("native result host", error);
                    return None;
                }
            };
            self.configure_child(&mut agent.command);
            agent.command.current_dir(&self.root);
            if !self.reserve_turn().await {
                return None;
            }
            let started = Instant::now();
            let child = self
                .supervisor
                .run_request(
                    agent.command,
                    Some(timeout),
                    output.capture_limit,
                    Some(&agent.input),
                    self.host_output,
                )
                .await;
            let duration = started.elapsed().as_millis() as u64;
            metrics.agent_duration_ms = Some(duration);
            self.result.semantic_duration_ms += duration;
            let child = match child {
                Ok(child) => child,
                Err(error) => match self.unstarted_call(metrics, error) {
                    Ok(reason) => return Some(ResultReply::HostUnavailable(reason)),
                    Err(error) => {
                        metrics.outcome = WorkOutcome::Failed;
                        self.failure("native result host", error);
                        return None;
                    }
                },
            };
            metrics.host_end = Self::host_end(metrics, child.finished_at);
            if child.cancelled {
                metrics.outcome = WorkOutcome::Cancelled;
                self.record("native result host", StageState::Cancelled, child.detail);
                return None;
            }
            if !self.configuration_is_current() {
                // The reply answered a configuration the run no longer holds.
                metrics.outcome = WorkOutcome::InputsChanged;
                return None;
            }
            if !child.success {
                metrics.outcome = WorkOutcome::Failed;
                metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                    self.agent_profile(metrics).engine(),
                    execution::diagnostic::Phase::Invocation,
                    &child,
                ));
                let engine = self.agent_profile(metrics).engine();
                if let Some(code) = child.failed_exit() {
                    metrics.unanswered = true;
                    return Some(ResultReply::InvocationFailed(
                        crate::commands::work::state::InvocationFailure::Exited {
                            engine,
                            code,
                            host: metrics.host_end.clone(),
                        },
                    ));
                }
                if child.termination == execution::ChildTermination::TimedOut {
                    metrics.unanswered = true;
                    return Some(ResultReply::InvocationFailed(
                        crate::commands::work::state::InvocationFailure::TimedOut {
                            engine,
                            host: metrics.host_end.clone(),
                        },
                    ));
                }
                self.failure(
                    "native result host",
                    miette::miette!("{}", child.detail.unwrap_or_else(|| "host failed".into())),
                );
                return None;
            }
            match output.response(
                &child.stdout,
                request["max_response_bytes"]
                    .as_u64()
                    .and_then(|limit| usize::try_from(limit).ok())
                    .expect("native response allowance"),
                metrics.host_end.clone(),
            ) {
                Ok(value) => {
                    metrics.answered = true;
                    metrics.result_json_bytes = Some(
                        serde_json::to_vec(&value)
                            .expect("response serializes")
                            .len(),
                    );
                    Some(ResultReply::Complete(value))
                }
                Err(error) => {
                    metrics.outcome = WorkOutcome::Failed;
                    metrics.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                        self.agent_profile(metrics).engine(),
                        execution::diagnostic::Phase::Response,
                        &child,
                    ));
                    if let execution::result::ResponseError::Remote { failure, .. } = &error {
                        metrics.answered = !failure.is_unanswered();
                    }
                    if let execution::result::ResponseError::Remote { failure, .. } = &error
                        && (matches!(access, execution::AgentAccess::ReadOnly)
                            || failure.is_unanswered())
                    {
                        metrics.unanswered = failure.is_unanswered();
                        return Some(ResultReply::InvocationFailed(failure.clone()));
                    }
                    self.failure("native result response", typed_error(error));
                    None
                }
            }
        }

        async fn finalize_result(
            &mut self,
            id: &str,
            result: crate::commands::work::ResultFinalization,
            metrics: &mut WorkStepMetrics,
        ) -> WorkStep {
            let schema = if metrics.stages == [ExecutionStage::Audit] {
                execution::audit::schema()
            } else {
                crate::commands::work::generation::schema()
            };
            let response = match self.request_result(&result.request, &schema, metrics).await {
                Some(ResultReply::Complete(response)) => response,
                Some(ResultReply::HostUnavailable(reason)) => {
                    return self.defer_turn_for_host(&reason);
                }
                Some(ResultReply::InvocationFailed(failure)) => {
                    return self
                        .hold_failed_request(metrics, |runner, session| {
                            crate::commands::work::hold_result(
                                session,
                                &runner.config,
                                &runner.config_revision,
                                id,
                                &result,
                                failure,
                                runner.supervisor.read_control(),
                            )
                            .map(Vec::from_iter)
                        })
                        .await;
                }
                None => return WorkStep::Failed,
            };
            let reply = match crate::commands::work::generation::replies(response, 1) {
                Ok(mut replies) => replies.remove(0),
                Err(error) => {
                    self.failure("native finalization response", error);
                    return WorkStep::Failed;
                }
            };
            let started = Instant::now();
            let Some((session, _)) = self.mutation_session("native finalization").await else {
                return WorkStep::Failed;
            };
            let targets = self.finalization_targets();
            let finished = crate::commands::work::finish_result(
                &session,
                &self.config,
                &self.config_revision,
                id,
                result,
                reply,
                self.finalization_replans.review_allowance(&targets),
                self.supervisor.read_control(),
            );
            metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
            match finished {
                Ok(crate::commands::work::ResultFinish::Rejected(reason)) => {
                    metrics.outcome = WorkOutcome::Rejected;
                    self.record(
                        "native finalization rejection",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    WorkStep::Deferred(reason)
                }
                Ok(crate::commands::work::ResultFinish::NeedsAction(reason)) => {
                    metrics.outcome = WorkOutcome::NeedsAction;
                    self.result.work_telemetry.retained_progress.action_receipts += 1;
                    self.record(
                        "native finalization action",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    WorkStep::Deferred(reason)
                }
                Ok(crate::commands::work::ResultFinish::Staged(refused_review)) => {
                    self.record_refused_review(refused_review);
                    metrics.outcome = WorkOutcome::Staged;
                    self.record(
                        "bounded work completion",
                        StageState::Completed,
                        Some(id.into()),
                    );
                    WorkStep::Staged
                }
                Ok(crate::commands::work::ResultFinish::ReviewRecorded(path)) => {
                    metrics.outcome = WorkOutcome::Recorded;
                    self.result
                        .work_telemetry
                        .retained_progress
                        .synthesis_reviews += 1;
                    for target in &targets {
                        self.finalization_replans.spend_review(target);
                    }
                    self.record("synthesis review", StageState::Completed, Some(path));
                    self.queue_admitted.clear();
                    self.queue_turn = None;
                    self.queue_replan = QueueReplan::Invalidate;
                    WorkStep::Replan
                }
                Ok(crate::commands::work::ResultFinish::ReviewDeferred(reason)) => {
                    metrics.outcome = WorkOutcome::ReviewDeferred;
                    for target in &targets {
                        self.finalization_replans.defer(target, &reason);
                    }
                    self.record(
                        "synthesis review",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    WorkStep::Deferred(reason)
                }
                Ok(crate::commands::work::ResultFinish::DiscoveryRecorded) => {
                    metrics.outcome = WorkOutcome::Recorded;
                    self.result
                        .work_telemetry
                        .retained_progress
                        .discovery_recipes += 1;
                    self.record(
                        "extraction discovery recovery",
                        StageState::Skipped,
                        Some(id.into()),
                    );
                    self.queue_admitted.clear();
                    self.queue_turn = None;
                    self.queue_replan = QueueReplan::Invalidate;
                    WorkStep::Replan
                }
                Err(error) if is_semantic_context_changed(&error) => {
                    metrics.outcome = WorkOutcome::InputsChanged;
                    if metrics.stages == [ExecutionStage::Audit] {
                        self.record(
                            "bounded audit inputs changed",
                            StageState::Skipped,
                            Some(error.to_string()),
                        );
                        WorkStep::Replan
                    } else {
                        self.finalization_inputs_changed(
                            "bounded finalization inputs changed",
                            error,
                        )
                    }
                }
                Err(error) => {
                    self.failure("native finalization", error);
                    WorkStep::Failed
                }
            }
        }

        /// Discards a queue finalizer step whose fresh guard found changed inputs; its answer, if
        /// any, is never relabeled. Each target may replan once per run, then defers.
        /// Bounded Work whose inputs changed before or after its host: audit Work replans under
        /// the per-subject allowance its caller spends, queue Work under the per-target one.
        fn work_inputs_changed(
            &mut self,
            stage: ExecutionStage,
            error: miette::Report,
        ) -> WorkStep {
            if stage != ExecutionStage::Audit {
                return self.finalization_inputs_changed("bounded work inputs changed", error);
            }
            self.record(
                "bounded audit inputs changed",
                StageState::Skipped,
                Some(error.to_string()),
            );
            WorkStep::Replan
        }

        /// The queue targets the bounded finalization in flight answers.
        /// A refused review proposal is reported, never written to the vault: the synthesis its
        /// reply carried was staged without it.
        fn record_refused_review(&mut self, refusal: Option<String>) {
            if let Some(refusal) = refusal {
                self.record(
                    "synthesis review refused",
                    StageState::Skipped,
                    Some(refusal),
                );
            }
        }

        fn finalization_targets(&self) -> Vec<String> {
            self.queue_turn.as_ref().map_or_else(
                || self.reserved_targets.clone(),
                |turn| turn.targets.clone(),
            )
        }

        fn finalization_inputs_changed(&mut self, name: &str, error: miette::Report) -> WorkStep {
            self.record(name, StageState::Skipped, Some(error.to_string()));
            let targets = self.finalization_targets();
            let mut replan = true;
            for target in &targets {
                if !self
                    .finalization_replans
                    .try_spend(target, FINALIZATION_CHANGED_AGAIN)
                {
                    self.deferred("finalization inputs unstable", target);
                    replan = false;
                }
            }
            if replan {
                WorkStep::Replan
            } else {
                WorkStep::Deferred(FINALIZATION_CHANGED_AGAIN.into())
            }
        }

        /// Records holds for a tool-enabled request whose host failed, then ends this run like a
        /// read wave with only invocation failures. `hold` returns one reason per held target;
        /// targets whose inputs changed are not held and get fresh preparation in a later run.
        async fn hold_failed_request(
            &mut self,
            metrics: &mut WorkStepMetrics,
            hold: impl FnOnce(&mut Self, &lk_vault::MutationSession) -> miette::Result<Vec<String>>,
        ) -> WorkStep {
            let started = Instant::now();
            let Some((session, _)) = self.mutation_session("native result invocation hold").await
            else {
                return WorkStep::Failed;
            };
            let held = hold(self, &session);
            drop(session);
            metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
            let step = match held {
                Ok(reasons) if reasons.is_empty() => {
                    self.deferred(
                        "native result inputs changed",
                        "inputs changed after the failed invocation; no hold was recorded and fresh preparation replaces the request",
                    );
                    WorkStep::Replan
                }
                Ok(reasons) => {
                    for reason in &reasons {
                        self.deferred("native result invocation held", reason);
                    }
                    WorkStep::Deferred(reasons.join("; "))
                }
                Err(error) => {
                    self.failure("native result invocation hold", error);
                    return WorkStep::Failed;
                }
            };
            self.replan_after_hold();
            step
        }

        async fn selected_result_step(
            &mut self,
            tasks: &[lk_queue::QueueTask],
            inputs: &[serde_json::Value],
        ) -> WorkStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
            metrics.stages = tasks
                .iter()
                .map(|task| generation_stage(task.kind))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let started = Instant::now();
            let request =
                match crate::commands::work::native::selected_request(&self.config, inputs) {
                    Ok(request) => request,
                    Err(error) => {
                        self.failure("native selected request", error);
                        return WorkStep::Failed;
                    }
                };
            metrics.preparation_duration_ms = Some(started.elapsed().as_millis() as u64);
            let value = match self
                .request_result(
                    &request,
                    &crate::commands::work::generation::schema(),
                    &mut metrics,
                )
                .await
            {
                Some(ResultReply::Complete(value)) => value,
                Some(ResultReply::HostUnavailable(reason)) => {
                    self.record_work_step(metrics);
                    return self.defer_turn_for_host(&reason);
                }
                Some(ResultReply::InvocationFailed(failure)) => {
                    let step = self
                        .hold_failed_request(&mut metrics, |runner, session| {
                            let mut held = Vec::new();
                            for input in inputs {
                                held.extend(
                                    crate::commands::work::generation::hold_selected_controlled(
                                        session,
                                        &runner.config,
                                        &runner.config_revision,
                                        &request,
                                        input,
                                        failure.clone(),
                                        runner.supervisor.read_control(),
                                    )?,
                                );
                            }
                            Ok(held)
                        })
                        .await;
                    self.record_work_step(metrics);
                    return step;
                }
                None => {
                    self.record_work_step(metrics);
                    return WorkStep::Failed;
                }
            };
            let replies = match crate::commands::work::generation::replies(value, inputs.len()) {
                Ok(replies) => replies,
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("native selected response", error);
                    return WorkStep::Failed;
                }
            };
            let started = Instant::now();
            let Some((session, _)) = self.mutation_session("native selected result").await else {
                self.record_work_step(metrics);
                return WorkStep::Failed;
            };
            let mut deferred = false;
            let mut failed = false;
            let mut staged = false;
            let mut recovered = false;
            let mut needs_action = false;
            let mut review_deferred = false;
            let mut rejected = false;
            let mut inputs_changed = false;
            for (input, reply) in inputs.iter().zip(replies) {
                let target = input["task"]["target"]["vault_path"]
                    .as_str()
                    .expect("selected task target")
                    .to_owned();
                match crate::commands::work::generation::finish_selected_controlled(
                    &session,
                    &self.config,
                    &self.config_revision,
                    &request,
                    input,
                    reply,
                    self.finalization_replans
                        .review_allowance(std::slice::from_ref(&target)),
                    self.supervisor.read_control(),
                ) {
                    Ok(crate::commands::work::generation::SelectedFinish::DiscoveryRecorded) => {
                        self.result
                            .work_telemetry
                            .retained_progress
                            .discovery_recipes += 1;
                        recovered = true;
                        self.record(
                            "extraction discovery recovery",
                            StageState::Skipped,
                            input["task"]["task_id"].as_str().map(str::to_owned),
                        );
                    }
                    Ok(crate::commands::work::generation::SelectedFinish::ReviewRecorded(path)) => {
                        self.result
                            .work_telemetry
                            .retained_progress
                            .synthesis_reviews += 1;
                        recovered = true;
                        self.finalization_replans.spend_review(&target);
                        self.record("synthesis review", StageState::Completed, Some(path));
                    }
                    Ok(crate::commands::work::generation::SelectedFinish::ReviewDeferred(
                        reason,
                    )) => {
                        deferred = true;
                        review_deferred = true;
                        self.finalization_replans.defer(&target, &reason);
                        self.record("synthesis review", StageState::Skipped, Some(reason));
                    }
                    Ok(crate::commands::work::generation::SelectedFinish::NeedsAction(reason)) => {
                        self.result.work_telemetry.retained_progress.action_receipts += 1;
                        deferred = true;
                        needs_action = true;
                        self.record("native selected action", StageState::Skipped, Some(reason));
                    }
                    Ok(crate::commands::work::generation::SelectedFinish::Staged(
                        refused_review,
                    )) => {
                        self.record_refused_review(refused_review);
                        staged = true;
                        self.record(
                            "native selected result",
                            StageState::Completed,
                            Some(
                                input["task"]["task_id"]
                                    .as_str()
                                    .expect("selected task")
                                    .into(),
                            ),
                        );
                    }
                    Ok(crate::commands::work::generation::SelectedFinish::ScopeChanged) => {
                        deferred = true;
                        inputs_changed = true;
                        self.record("expanded extraction work", StageState::Skipped, Some("selected scope changed; old semantic reply was not adopted; preparing fresh work".into()));
                        if !self
                            .finalization_replans
                            .try_spend(&target, FINALIZATION_CHANGED_AGAIN)
                        {
                            self.deferred("finalization inputs unstable", &target);
                        }
                    }
                    Ok(crate::commands::work::generation::SelectedFinish::Rejected(reason)) => {
                        deferred = true;
                        rejected = true;
                        self.record(
                            "native selected rejection",
                            StageState::Skipped,
                            Some(reason),
                        );
                    }
                    Err(error) => {
                        failed = true;
                        self.failure("native selected result", error);
                    }
                }
            }
            metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
            metrics.outcome = if staged {
                WorkOutcome::Staged
            } else if recovered {
                WorkOutcome::Recorded
            } else if failed {
                WorkOutcome::Failed
            } else if needs_action {
                WorkOutcome::NeedsAction
            } else if review_deferred {
                WorkOutcome::ReviewDeferred
            } else if inputs_changed {
                WorkOutcome::InputsChanged
            } else if rejected {
                WorkOutcome::Rejected
            } else {
                WorkOutcome::Unsettled
            };
            self.record_work_step(metrics);
            if failed {
                WorkStep::Failed
            } else if recovered {
                self.queue_admitted.clear();
                self.queue_turn = None;
                self.queue_replan = QueueReplan::Invalidate;
                WorkStep::Replan
            } else if deferred {
                WorkStep::Deferred("selected tasks require explicit actions".into())
            } else {
                WorkStep::Staged
            }
        }

        async fn whole_section_step(&mut self, task: &lk_queue::QueueTask) -> WorkStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
            let Some((session, waited)) = self.mutation_session("whole-section preparation").await
            else {
                return WorkStep::Failed;
            };
            if waited {
                self.refresh_queue_inputs();
                return WorkStep::Failed;
            }
            let started = Instant::now();
            let prepared = crate::commands::work::section::prepare_controlled(
                &session,
                &self.config,
                &self.config_revision,
                &task.task_id,
                self.supervisor.read_control(),
            );
            drop(session);
            metrics.preparation_duration_ms = Some(started.elapsed().as_millis() as u64);
            let section = match prepared {
                Ok(crate::commands::work::section::Preparation::Ready(section)) => section,
                Ok(crate::commands::work::section::Preparation::NeedsAction(reason)) => {
                    metrics.outcome = WorkOutcome::NeedsAction;
                    self.record_work_step(metrics);
                    return WorkStep::Deferred(reason);
                }
                Ok(crate::commands::work::section::Preparation::Capacity {
                    request_bytes,
                    max_input_bytes,
                }) => {
                    self.record_work_step(metrics);
                    return WorkStep::Deferred(format!(
                        "complete section request needs {request_bytes} bytes, above max_input_bytes {max_input_bytes}; retained for explicit capacity adjustment"
                    ));
                }
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("whole-section preparation", error);
                    return WorkStep::Failed;
                }
            };
            let result = self.invoke_whole_section(*section, &mut metrics).await;
            self.record_work_step(metrics);
            result
        }

        async fn invoke_whole_section(
            &mut self,
            section: crate::commands::work::section::Section,
            metrics: &mut WorkStepMetrics,
        ) -> WorkStep {
            let response = match self
                .request_observation(section.request.clone(), metrics)
                .await
            {
                Some(ObservationReply::HostUnavailable(reason)) => {
                    return self.defer_turn_for_host(&reason);
                }
                Some(ObservationReply::InvocationFailed(failure)) => Err(failure),
                Some(ObservationReply::Output { engine, child, .. }) => {
                    let bytes = child.stdout.as_slice();
                    match execution::finalization::decode(
                        engine,
                        bytes,
                        section.max_response_bytes,
                        &section.request,
                    )
                    .and_then(|(response, size)| {
                        section.validate_response(&response)?;
                        Ok((response, size))
                    }) {
                        Ok((response, size)) => {
                            metrics.answered = true;
                            metrics.result_json_bytes = Some(size);
                            Ok(response)
                        }
                        Err(error) => {
                            metrics.result_json_bytes = error
                                .downcast_ref::<crate::commands::work::state::OutputCapacity>()
                                .map(|capacity| capacity.actual_bytes);
                            metrics.host_failure =
                                Some(execution::diagnostic::HostFailure::from_child(
                                    engine,
                                    execution::diagnostic::Phase::Response,
                                    &child,
                                ));
                            let failure = execution::observation::response_failure(
                                engine,
                                bytes,
                                metrics.host_end.clone(),
                            );
                            metrics.unanswered = failure.is_unanswered();
                            metrics.answered = !metrics.unanswered;
                            Err(failure)
                        }
                    }
                }
                None => return WorkStep::Failed,
            };
            let started = Instant::now();
            let Some((session, _)) = self.mutation_session("whole-section publication").await
            else {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return WorkStep::Failed;
            };
            let response = match response {
                Ok(response) => response,
                Err(failure) => {
                    let result = crate::commands::work::section::hold(
                        &session,
                        &self.config,
                        &self.config_revision,
                        section,
                        failure,
                    );
                    metrics.revalidation_record_duration_ms =
                        Some(started.elapsed().as_millis() as u64);
                    metrics.outcome = WorkOutcome::Failed;
                    return match result {
                        Ok(Some(reason)) => {
                            self.deferred("whole-section invocation held", &reason);
                            self.replan_after_hold();
                            WorkStep::Deferred(reason)
                        }
                        Ok(None) => {
                            self.deferred(
                                "whole-section inputs changed",
                                "inputs changed after the failed invocation; no hold was recorded and fresh preparation replaces the request",
                            );
                            self.replan_after_hold();
                            WorkStep::Replan
                        }
                        Err(error) => {
                            self.failure("whole-section attempt recording", error);
                            WorkStep::Failed
                        }
                    };
                }
            };
            let held = if matches!(
                response,
                execution::finalization::SectionResponse::InvalidBody(_)
            ) {
                WorkOutcome::Rejected
            } else {
                WorkOutcome::NeedsAction
            };
            let result = crate::commands::work::section::finish(
                &session,
                &self.config,
                &self.config_revision,
                section,
                response,
            );
            metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
            match result {
                Ok(Some(reason)) => {
                    metrics.outcome = held;
                    self.result.work_telemetry.retained_progress.action_receipts += 1;
                    self.record(
                        "whole-section action",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    WorkStep::Deferred(reason)
                }
                Ok(None) => {
                    metrics.outcome = WorkOutcome::Staged;
                    self.record(
                        "whole-section result",
                        StageState::Completed,
                        Some("guarded result staged".into()),
                    );
                    WorkStep::Staged
                }
                Err(error) => {
                    self.failure("whole-section publication", error);
                    WorkStep::Failed
                }
            }
        }

        async fn finalize_section(
            &mut self,
            id: &str,
            section: crate::commands::work::SectionFinalization,
            metrics: &mut WorkStepMetrics,
        ) -> WorkStep {
            let value = match self
                .request_result(
                    &section.request,
                    &execution::finalization::schema(),
                    metrics,
                )
                .await
            {
                Some(ResultReply::Complete(value)) => value,
                Some(ResultReply::HostUnavailable(reason)) => {
                    return self.defer_turn_for_host(&reason);
                }
                Some(ResultReply::InvocationFailed(failure)) => {
                    return self
                        .hold_failed_request(metrics, |runner, session| {
                            crate::commands::work::hold_section(
                                session,
                                &runner.config,
                                &runner.config_revision,
                                id,
                                &section,
                                failure,
                                runner.supervisor.read_control(),
                            )
                            .map(Vec::from_iter)
                        })
                        .await;
                }
                None => return WorkStep::Failed,
            };
            let (response, bytes) =
                match execution::finalization::decode_value(value, &section.request) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        metrics.outcome = WorkOutcome::Rejected;
                        self.failure("bounded section finalization", error);
                        return WorkStep::Failed;
                    }
                };
            metrics.result_json_bytes = Some(bytes);
            let started_at = Instant::now();
            let Some((session, _)) = self.mutation_session("bounded section finalization").await
            else {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return WorkStep::Failed;
            };
            let (finished, action) = match response {
                execution::finalization::SectionResponse::Complete(body) => (
                    crate::commands::work::finish_section(
                        &session,
                        &self.config,
                        &self.config_revision,
                        id,
                        section,
                        body,
                        self.supervisor.read_control(),
                    ),
                    None,
                ),
                execution::finalization::SectionResponse::NeedsAction(reason) => (
                    crate::commands::work::record_finalization_action(
                        &session,
                        &self.config,
                        &self.config_revision,
                        id,
                        section,
                        reason.clone(),
                        self.supervisor.read_control(),
                    ),
                    Some((reason, WorkOutcome::NeedsAction)),
                ),
                execution::finalization::SectionResponse::InvalidBody(reason) => (
                    crate::commands::work::record_finalization_action(
                        &session,
                        &self.config,
                        &self.config_revision,
                        id,
                        section,
                        reason.clone(),
                        self.supervisor.read_control(),
                    ),
                    Some((reason, WorkOutcome::Rejected)),
                ),
            };
            metrics.revalidation_record_duration_ms = Some(started_at.elapsed().as_millis() as u64);
            match finished {
                Ok(()) => match action {
                    Some((reason, outcome)) => {
                        metrics.outcome = outcome;
                        self.result.work_telemetry.retained_progress.action_receipts += 1;
                        self.record(
                            "bounded section action",
                            StageState::Skipped,
                            Some(reason.clone()),
                        );
                        WorkStep::Deferred(reason)
                    }
                    None => {
                        metrics.outcome = WorkOutcome::Staged;
                        self.record(
                            "bounded work completion",
                            StageState::Completed,
                            Some(id.into()),
                        );
                        WorkStep::Staged
                    }
                },
                Err(error) if is_semantic_context_changed(&error) => {
                    metrics.outcome = WorkOutcome::InputsChanged;
                    self.finalization_inputs_changed("bounded section inputs changed", error)
                }
                Err(error) => {
                    self.failure("bounded section finalization", error);
                    WorkStep::Failed
                }
            }
        }

        async fn bounded_step(&mut self, id: &str, stage: ExecutionStage) -> WorkStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Evidence);
            metrics.work_id = Some(id.into());
            let Some((session, waited)) = self.mutation_session("bounded work progress").await
            else {
                return WorkStep::Failed;
            };
            // Audit Work is revalidated against current inputs here; queue planning is not.
            if waited && stage != ExecutionStage::Audit {
                self.refresh_queue_inputs();
                return WorkStep::Failed;
            }
            let started_at = Instant::now();
            let prepared = crate::commands::work::prepare_native_step(
                &session,
                &self.config,
                &self.config_revision,
                id,
                self.supervisor.read_control(),
            );
            drop(session);
            let step = match prepared {
                Ok(step) => step,
                Err(error) => {
                    metrics.preparation_duration_ms = Some(started_at.elapsed().as_millis() as u64);
                    self.record_work_step(metrics);
                    if crate::commands::work::state::is_input_capacity(&error) {
                        let reason = error.to_string();
                        self.deferred("bounded work capacity", &reason);
                        return WorkStep::Deferred(reason);
                    }
                    if is_semantic_context_changed(&error) {
                        return self.work_inputs_changed(stage, error);
                    }
                    self.failure("bounded work progress", error);
                    return WorkStep::Failed;
                }
            };
            match step {
                crate::commands::work::NativeStep::Inspection {
                    progress,
                    request,
                    preparation_duration_ms,
                } => match self
                    .bounded_observation_step(id, progress, request, preparation_duration_ms)
                    .await
                {
                    ObservationStep::Recorded | ObservationStep::Refined => WorkStep::Pending,
                    ObservationStep::Deferred(reason) => WorkStep::Deferred(reason),
                    ObservationStep::InputsChanged(error) => self.work_inputs_changed(stage, error),
                    ObservationStep::InvocationFailed => {
                        self.replan_after_hold();
                        WorkStep::Deferred(format!("{id}: inspection invocation held"))
                    }
                    ObservationStep::Failed => WorkStep::Failed,
                },
                crate::commands::work::NativeStep::Finalization {
                    root_node,
                    checkpoint,
                } => {
                    metrics.role = WorkRole::Finalizer;
                    metrics.stages.push(stage);
                    metrics.node_id = Some(root_node);
                    metrics.preparation_duration_ms = Some(0);
                    self.bounded_finish_prepared(id, metrics, checkpoint).await
                }
            }
        }

        async fn bounded_finish(
            &mut self,
            id: &str,
            mut metrics: WorkStepMetrics,
            frontier: bool,
        ) -> WorkStep {
            let Some((session, waited)) = self.mutation_session("bounded work completion").await
            else {
                return WorkStep::Failed;
            };
            if waited {
                self.refresh_queue_inputs();
                return WorkStep::Failed;
            }
            let started_at = Instant::now();
            let prepared = crate::commands::work::prepare_native_finalization(
                &session,
                &self.config,
                &self.config_revision,
                id,
                frontier,
                self.supervisor.read_control(),
            );
            drop(session);
            let checkpoint = match prepared {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    *metrics
                        .preparation_duration_ms
                        .as_mut()
                        .expect("measured preparation") += started_at.elapsed().as_millis() as u64;
                    let changed = is_semantic_context_changed(&error);
                    if changed {
                        metrics.outcome = WorkOutcome::InputsChanged;
                    }
                    self.record_work_step(metrics);
                    if changed {
                        return self.finalization_inputs_changed(
                            "bounded finalization inputs changed",
                            error,
                        );
                    }
                    self.failure("bounded work completion", error);
                    return WorkStep::Failed;
                }
            };
            self.bounded_finish_prepared(id, metrics, checkpoint).await
        }

        async fn bounded_finish_prepared(
            &mut self,
            id: &str,
            mut metrics: WorkStepMetrics,
            checkpoint: crate::commands::work::FinalizationCheckpoint,
        ) -> WorkStep {
            *metrics
                .preparation_duration_ms
                .as_mut()
                .expect("measured preparation") += checkpoint.preparation_duration_ms;
            metrics.verification_duration_ms = Some(checkpoint.verification_duration_ms);
            let section = match checkpoint.state {
                crate::commands::work::NativeFinalization::Finished => {
                    let started_at = Instant::now();
                    let Some((session, _)) = self.mutation_session("bounded work completion").await
                    else {
                        if self.result.cancelled {
                            metrics.outcome = WorkOutcome::Cancelled;
                        }
                        self.record_work_step(metrics);
                        return WorkStep::Failed;
                    };
                    let restaged = crate::commands::work::restage_finished(
                        &session,
                        &self.config,
                        &self.config_revision,
                        id,
                        self.supervisor.read_control(),
                    );
                    metrics.revalidation_record_duration_ms =
                        Some(started_at.elapsed().as_millis() as u64);
                    if let Err(error) = restaged {
                        if let Some(reason) = crate::commands::audit::assessment_blocker(&error) {
                            self.record_work_step(metrics);
                            return WorkStep::Deferred(reason);
                        }
                        if let Some(held) =
                            error.downcast_ref::<crate::commands::queue::SynthesisHeld>()
                        {
                            self.record_work_step(metrics);
                            return WorkStep::Deferred(held.to_string());
                        }
                        self.record_work_step(metrics);
                        self.failure("bounded work completion", error);
                        return WorkStep::Failed;
                    }
                    metrics.outcome = WorkOutcome::Restaged;
                    self.record_work_step(metrics);
                    self.record(
                        "bounded work completion",
                        StageState::Skipped,
                        Some("verified sealed result already staged".into()),
                    );
                    return WorkStep::Staged;
                }
                crate::commands::work::NativeFinalization::Ready(section) => section,
                crate::commands::work::NativeFinalization::Replan => {
                    self.queue_replan = QueueReplan::Invalidate;
                    return WorkStep::Failed;
                }
            };
            match section {
                crate::commands::work::FinalizationPreparation::Result(result) => {
                    let result = self.finalize_result(id, *result, &mut metrics).await;
                    self.record_work_step(metrics);
                    result
                }
                crate::commands::work::FinalizationPreparation::Capacity {
                    request_bytes,
                    max_input_bytes,
                } => {
                    let reason = crate::commands::work::finalization_capacity_reason(
                        request_bytes,
                        max_input_bytes,
                    );
                    self.record(
                        "bounded section capacity",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    self.record_work_step(metrics);
                    WorkStep::Deferred(reason)
                }
                crate::commands::work::FinalizationPreparation::NeedsAction(reason) => {
                    metrics.outcome = WorkOutcome::NeedsAction;
                    self.record(
                        "bounded section action",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    self.record_work_step(metrics);
                    WorkStep::Deferred(reason)
                }
                crate::commands::work::FinalizationPreparation::Ready(section) => {
                    let result = self.finalize_section(id, *section, &mut metrics).await;
                    self.record_work_step(metrics);
                    result
                }
            }
        }

        fn prepare_queue_screenings(
            &mut self,
            session: &lk_vault::MutationSession,
            targets: &[String],
            jobs: &mut std::collections::BTreeMap<String, QueueDispatch>,
        ) -> miette::Result<Option<Vec<ScreeningAttempt>>> {
            if targets.is_empty() {
                return Ok(Some(Vec::new()));
            }
            let task_ids = targets
                .iter()
                .map(|target| match &jobs[target] {
                    QueueDispatch::Screening { task, .. } => task.task_id.as_str(),
                    _ => unreachable!("selected screening target"),
                })
                .collect::<Vec<_>>();
            let reuse = targets.iter().all(|target| {
                matches!(&jobs[target],
                QueueDispatch::Screening {prepared:Some(_), prepared_at_call, ..}
                    if *prepared_at_call==self.result.semantic_calls)
            });
            let started = Instant::now();
            let prepared = if reuse {
                targets
                    .iter()
                    .map(
                        |target| match jobs.get_mut(target).expect("selected target") {
                            QueueDispatch::Screening { prepared, .. } => {
                                *prepared.take().expect("prepared screening")
                            }
                            _ => unreachable!(),
                        },
                    )
                    .collect::<Vec<_>>()
            } else {
                let captured = crate::commands::work::prepare_screenings(
                    session,
                    &self.config,
                    &self.config_revision,
                    &task_ids,
                    self.supervisor.read_control(),
                )?;
                self.result
                    .work_telemetry
                    .screening_preparation_io
                    .accumulate(captured.discovery_io);
                captured.turns
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            let mut turns = Vec::new();
            for (index, (target, before)) in targets.iter().zip(prepared).enumerate() {
                let expected = jobs[target].binding(target).expect("screening binding");
                if expected.identity != format!("screening:{}", before.screening_id) {
                    self.queue_replan = QueueReplan::Invalidate;
                    return Ok(None);
                }
                let request = match before.state {
                    crate::commands::work::ScreeningState::Complete => {
                        self.queue_replan = QueueReplan::Invalidate;
                        return Ok(None);
                    }
                    crate::commands::work::ScreeningState::Deferred(reason) => {
                        self.queue_replan = QueueReplan::Invalidate;
                        self.deferred("candidate screening", reason);
                        return Ok(None);
                    }
                    crate::commands::work::ScreeningState::Ready(request) => request,
                };
                let task_id = match &jobs[target] {
                    QueueDispatch::Screening { task, .. } => task.task_id.clone(),
                    _ => unreachable!(),
                };
                let mut metrics = WorkStepMetrics::new(WorkRole::Screening);
                metrics.screening_id = Some(before.screening_id);
                metrics.request_revision = request["request_revision"].as_str().map(str::to_owned);
                metrics.preparation_from_queue_plan = reuse;
                metrics.preparation_duration_ms = (index == 0 && !reuse).then_some(duration_ms);
                metrics.request_json_bytes = Some(
                    serde_json::to_vec(&execution::observation::screening_request(&request)?)
                        .expect("native request serializes")
                        .len(),
                );
                turns.push(ScreeningAttempt {
                    task_id,
                    decided: before.decided,
                    candidates: before.candidates,
                    request,
                    metrics,
                });
            }
            Ok(Some(turns))
        }

        fn record_uninvoked_queue_reads(
            &mut self,
            turns: Vec<(String, QueueReadTurn)>,
            outcome: WorkOutcome,
        ) {
            for (_, turn) in turns {
                let mut metrics = turn.into_metrics();
                metrics.outcome = if self.result.cancelled {
                    WorkOutcome::Cancelled
                } else {
                    outcome
                };
                self.record_work_step(metrics);
            }
        }

        async fn queue_read_wave(
            &mut self,
            selected: &str,
            jobs: &mut std::collections::BTreeMap<String, QueueDispatch>,
            keys: &std::collections::BTreeSet<String>,
        ) -> bool {
            let budget = match self.config.execution.effective_work() {
                Ok(budget) => budget,
                Err(error) => return self.failure("semantic work budget", typed_error(error)),
            };
            let limit = self.wave_limit(budget);
            if limit == 0 {
                return self.call_budget_available();
            }
            let continued = jobs[selected].binding(selected).is_some_and(|binding| {
                self.queue_admitted
                    .can_continue(std::slice::from_ref(&binding))
            });
            let mut order = if continued {
                self.queue_admitted
                    .remaining_bindings()
                    .map(|binding| binding.target.clone())
                    .collect::<Vec<_>>()
            } else {
                match scheduler::order(&self.root, scheduler::Stage::Queue, keys) {
                    Ok(order) => order,
                    Err(error) => return self.failure("queue scheduling", error),
                }
            };
            let first = order
                .iter()
                .position(|target| target == selected)
                .expect("selected queue target");
            order.rotate_left(first);
            let targets = order
                .iter()
                .take(limit)
                .take_while(|target| {
                    matches!(
                        jobs.get(*target),
                        Some(
                            QueueDispatch::Screening { .. }
                                | QueueDispatch::Partitioned { .. }
                                | QueueDispatch::Source { .. }
                        )
                    )
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut planned = targets.clone();
            let mut demands = std::collections::BTreeMap::<String, usize>::new();
            for target in &targets {
                if let QueueDispatch::Partitioned { id, .. } | QueueDispatch::Source { id } =
                    &jobs[target]
                {
                    *demands.entry(id.clone()).or_default() += 1;
                }
            }
            if targets.len() == order.len() {
                while planned.len() < limit {
                    let before = planned.len();
                    for target in &targets {
                        if planned.len() == limit {
                            break;
                        }
                        let (QueueDispatch::Partitioned { id, .. } | QueueDispatch::Source { id }) =
                            &jobs[target]
                        else {
                            continue;
                        };
                        let binding = jobs[target].binding(target).expect("work binding");
                        let allowance = if continued {
                            self.queue_admitted.remaining(&binding)
                        } else {
                            budget.max_consecutive_calls
                        };
                        let count = demands.get_mut(id).expect("selected work demand");
                        if *count < allowance {
                            *count += 1;
                            planned.push(target.clone());
                        }
                    }
                    if planned.len() == before {
                        break;
                    }
                }
            }
            let screen_targets = targets
                .iter()
                .filter(|target| matches!(jobs[*target], QueueDispatch::Screening { .. }))
                .cloned()
                .collect::<Vec<_>>();
            let Some((session, waited)) = self.mutation_session("queue read preparation").await
            else {
                return false;
            };
            if waited {
                self.refresh_queue_inputs();
                return true;
            }
            let screenings = match self.prepare_queue_screenings(&session, &screen_targets, jobs) {
                Ok(Some(turns)) => turns,
                Ok(None) => {
                    self.queue_admitted.clear();
                    return true;
                }
                Err(error) => return self.failure("candidate screening", error),
            };
            let mut screening_turns = screen_targets
                .into_iter()
                .zip(screenings)
                .collect::<std::collections::BTreeMap<_, _>>();
            let demands = demands.into_iter().collect::<Vec<_>>();
            let mut prepared = std::collections::BTreeMap::new();
            let mut preparation_duration_ms = None;
            if !demands.is_empty() {
                let captured = match crate::commands::work::prepare_inspections_batch(
                    &session,
                    &self.config,
                    &self.config_revision,
                    &demands,
                    self.supervisor.read_control(),
                ) {
                    Ok(captured) => captured,
                    Err(error) => return self.failure("bounded work progress", error),
                };
                self.result.work_telemetry.evidence_preparation_duration_ms += captured.duration_ms;
                preparation_duration_ms = Some(captured.duration_ms);
                for ((id, _), result) in demands.into_iter().zip(captured.turns) {
                    let (progress, requests) = match result {
                        Ok(captured) => captured,
                        Err(error) if crate::commands::work::state::is_input_capacity(&error) => {
                            self.deferred("bounded work capacity", error);
                            self.queue_replan = QueueReplan::Invalidate;
                            self.queue_admitted.clear();
                            return true;
                        }
                        Err(error) if is_semantic_context_changed(&error) => {
                            self.record(
                                "bounded work context",
                                StageState::Skipped,
                                Some(error.to_string()),
                            );
                            // Each target reading this Work spends its replan; one whose inputs
                            // change again is deferred, so its peers proceed.
                            let readers = jobs
                                .iter()
                                .filter_map(|(target, job)| match job {
                                    QueueDispatch::Partitioned { id: current, .. }
                                    | QueueDispatch::Source { id: current }
                                        if current == &id =>
                                    {
                                        Some(target.clone())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            for target in &readers {
                                if !self
                                    .finalization_replans
                                    .try_spend(target, EVIDENCE_CHANGED_AGAIN)
                                {
                                    self.deferred("bounded work inputs unstable", target);
                                }
                            }
                            self.queue_replan = QueueReplan::Invalidate;
                            self.queue_admitted.clear();
                            return true;
                        }
                        Err(error) => return self.failure("bounded work progress", error),
                    };
                    if progress.complete {
                        let completed = jobs
                            .iter()
                            .filter_map(|(target, job)| match job {
                                QueueDispatch::Partitioned { id: current, task }
                                    if current == &id =>
                                {
                                    Some((target.clone(), task.clone()))
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        if completed.is_empty() {
                            let mut metrics = WorkStepMetrics::new(WorkRole::Evidence);
                            metrics.work_id = Some(id);
                            metrics.preparation_duration_ms = preparation_duration_ms;
                            metrics.outcome = WorkOutcome::AlreadyComplete;
                            self.record_work_step(metrics);
                            self.queue_replan = QueueReplan::Invalidate;
                            self.queue_admitted.clear();
                        } else {
                            for (target, task) in completed {
                                jobs.insert(
                                    target,
                                    QueueDispatch::RootFinalization {
                                        id: id.clone(),
                                        task,
                                    },
                                );
                            }
                        }
                        return true;
                    }
                    if requests.is_empty() {
                        return self.failure(
                            "bounded work progress",
                            miette::miette!("incomplete work has no ready node"),
                        );
                    }
                    prepared.insert(
                        id,
                        PreparedObservations {
                            completed: progress.completed,
                            requests: requests.into(),
                        },
                    );
                }
            }
            let mut turns = Vec::new();
            for target in planned {
                let turn = match &jobs[&target] {
                    QueueDispatch::Screening { .. } => QueueReadTurn::Screening(
                        screening_turns
                            .remove(&target)
                            .expect("one screening per target"),
                    ),
                    QueueDispatch::Partitioned { id, .. } | QueueDispatch::Source { id } => {
                        let work = prepared.get_mut(id).expect("prepared work");
                        let Some(request) = work.requests.pop_front() else {
                            continue;
                        };
                        match crate::commands::work::inspection_action(session.root(), id, &request)
                        {
                            Ok(Some(reason)) => {
                                self.inspection_deferred.insert(id.clone(), reason.clone());
                                self.deferred("bounded work action", reason);
                                self.queue_replan = QueueReplan::Invalidate;
                                break;
                            }
                            Ok(None) => {}
                            Err(error) => return self.failure("bounded work action", error),
                        }
                        let node_id = request["node_id"]
                            .as_str()
                            .expect("prepared node identity")
                            .to_owned();
                        let mut metrics = WorkStepMetrics::new(WorkRole::Evidence);
                        metrics.work_id = Some(id.clone());
                        metrics.node_id = Some(node_id.clone());
                        metrics.node_kind = Some(match request["kind"].as_str() {
                            Some("inspect") => NodeKind::Inspect,
                            Some("reduce") => NodeKind::Reduce,
                            _ => unreachable!("prepared node kind"),
                        });
                        metrics.preparation_duration_ms = preparation_duration_ms.take();
                        QueueReadTurn::Evidence(ObservationTurn {
                            target: Some(target.clone()),
                            audit_subject: None,
                            work_id: id.clone(),
                            node_id,
                            completed: work.completed,
                            request,
                            metrics,
                        })
                    }
                    _ => unreachable!("read-only queue prefix"),
                };
                turns.push((target, turn));
            }
            drop(session);
            if turns.is_empty() {
                return true;
            }
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                self.record_uninvoked_queue_reads(turns, WorkOutcome::Unsent);
                return false;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                self.record_uninvoked_queue_reads(turns, WorkOutcome::Failed);
                return self.failure(
                    "bounded queue receipt",
                    miette::miette!("semantic execution is disabled by llm.provider"),
                );
            }
            let mut requests = Vec::new();
            let mut directories = Vec::new();
            let mut engines = Vec::new();
            // A turn routed to a host this run tripped is deferred alone; the wave still sends
            // the turns of healthy hosts.
            let planned = turns
                .iter()
                .map(|(target, _)| target.clone())
                .collect::<Vec<_>>();
            let mut ready = Vec::new();
            let mut unavailable_hosts = false;
            let mut pending = turns.into_iter();
            while let Some((target, mut turn)) = pending.next() {
                if let Err(error) = self.prepare_agent(turn.metrics_mut()).await {
                    if let Some(unavailable) = error.downcast_ref::<HostUnavailable>() {
                        let reason = unavailable.to_string();
                        self.finalization_replans.defer(&target, &reason);
                        self.deferred("host unavailable", format!("{target}: {reason}"));
                        self.record_uninvoked_queue_reads(
                            vec![(target, turn)],
                            WorkOutcome::Unsent,
                        );
                        unavailable_hosts = true;
                        continue;
                    }
                    self.record_uninvoked_queue_reads(
                        ready
                            .into_iter()
                            .chain([(target, turn)])
                            .chain(pending)
                            .collect(),
                        WorkOutcome::Failed,
                    );
                    return self.agent_preflight_failure("bounded queue receipt", error);
                }
                let timeout = self.agent_profile(turn.metrics()).timeout();
                let (invocation, limit, workload) = match self.observation_invocation(
                    turn.request(),
                    turn.role(),
                    turn.metrics(),
                    budget,
                ) {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        self.record_uninvoked_queue_reads(
                            ready
                                .into_iter()
                                .chain([(target, turn)])
                                .chain(pending)
                                .collect(),
                            WorkOutcome::Failed,
                        );
                        return self.failure("bounded queue receipt", error);
                    }
                };
                turn.metrics_mut().request_json_bytes = Some(invocation.request_json_bytes);
                turn.metrics_mut().workload = workload;
                engines.push(invocation.engine);
                let (request, directory) = invocation.into_request(timeout, limit);
                requests.push(request);
                directories.push(directory);
                ready.push((target, turn));
            }
            let turns = ready;
            if turns.is_empty() {
                self.replan_after_hold();
                return true;
            }
            if !self.admits_calls(requests.len()) {
                self.record_uninvoked_queue_reads(turns, WorkOutcome::Unsent);
                return false;
            }
            let remaining_calls = self.remaining_calls(budget);
            {
                let Some((session, waited)) = self.mutation_session("queue scheduling").await
                else {
                    self.record_uninvoked_queue_reads(turns, WorkOutcome::Failed);
                    return false;
                };
                if waited {
                    self.refresh_queue_inputs();
                    self.record_uninvoked_queue_reads(turns, WorkOutcome::Unsent);
                    return true;
                }
                // The calls are reserved before the cursor passes their targets, so a wave
                // the budget refuses keeps its place.
                if !self.reserve_calls(requests.len()) {
                    self.record_uninvoked_queue_reads(turns, WorkOutcome::Unsent);
                    return false;
                }
                let bindings = turns
                    .iter()
                    .map(|(target, _)| jobs[target].binding(target).expect("read-only binding"))
                    .collect::<Vec<_>>();
                if !self.queue_admitted.can_continue(&bindings) {
                    let mut seen = std::collections::BTreeSet::new();
                    let admitted = bindings
                        .iter()
                        .filter(|binding| seen.insert(binding.target.clone()))
                        .cloned()
                        .collect::<Vec<_>>();
                    // The cursor passes only targets this wave sends: a target none of whose
                    // turns was sent leaves the eligible set, and the reservation is checked
                    // against the rest, in the order the wave was planned.
                    let unsent = planned
                        .iter()
                        .filter(|target| !turns.iter().any(|(sent, _)| sent == *target))
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>();
                    let eligible = keys
                        .difference(&unsent)
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>();
                    let mut reserved = std::collections::BTreeSet::new();
                    let targets = planned
                        .iter()
                        .filter(|target| !unsent.contains(*target))
                        .filter(|target| reserved.insert((*target).clone()))
                        .cloned()
                        .collect::<Vec<_>>();
                    if let Err(error) = scheduler::reserve_in_session(
                        &session,
                        scheduler::Stage::Queue,
                        &eligible,
                        &targets,
                    ) {
                        return self.failure("queue scheduling", error);
                    }
                    self.queue_admitted.clear();
                    if let Err(error) = self.queue_admitted.admit(&admitted) {
                        return self.failure("queue scheduling", error);
                    }
                }
                if let Err(error) = self.queue_admitted.charge(&bindings) {
                    return self.failure("queue scheduling", error);
                }
                if unavailable_hosts {
                    self.replan_after_hold();
                }
            }
            eprintln!(
                "{}",
                serde_json::json!({
                    "type":"lore.queue-read-wave",
                    "request_count":requests.len(),
                    "evidence_requests":turns.iter().filter(|(_,turn)|matches!(turn,QueueReadTurn::Evidence(_))).count(),
                    "screening_requests":turns.iter().filter(|(_,turn)|matches!(turn,QueueReadTurn::Screening(_))).count(),
                    "max_parallel":budget.max_parallel,
                    "remaining_calls":remaining_calls,
                    "elapsed_job_ms":self.started_at.elapsed().as_millis(),
                })
            );
            let attempts = self
                .supervisor
                .run_requests(requests, self.host_output)
                .await;
            drop(directories);
            let cancelled = attempts
                .iter()
                .any(|attempt| attempt.result.as_ref().is_ok_and(|child| child.cancelled));
            let mut observations = Vec::new();
            let mut observed = Vec::new();
            let mut observation_engines = Vec::new();
            let mut screenings = Vec::new();
            let mut screened = Vec::new();
            let mut screening_engines = Vec::new();
            for (((_, turn), attempt), engine) in turns.into_iter().zip(attempts).zip(engines) {
                match turn {
                    QueueReadTurn::Evidence(turn) => {
                        observations.push(turn);
                        observed.push(attempt);
                        observation_engines.push(engine);
                    }
                    QueueReadTurn::Screening(turn) => {
                        screenings.push(turn);
                        screened.push(attempt);
                        screening_engines.push(engine);
                    }
                }
            }
            let has_screenings = !screenings.is_empty();
            let has_observations = !observations.is_empty();
            if has_screenings && has_observations {
                let screenings = self.decode_screening_attempts(
                    screenings,
                    screened,
                    screening_engines,
                    budget.max_result_bytes,
                    cancelled,
                );
                let observations = self.decode_observation_attempts(
                    observations,
                    observed,
                    observation_engines,
                    budget.max_result_bytes,
                    cancelled,
                );
                return self.settle_read_wave(screenings, observations).await;
            }
            let screening_ok = if !has_screenings {
                true
            } else {
                self.settle_screening_attempts(
                    screenings,
                    screened,
                    screening_engines,
                    budget.max_result_bytes,
                    cancelled,
                )
                .await
            };
            let observation_ok = if !has_observations {
                true
            } else {
                self.settle_observation_attempts(
                    observations,
                    observed,
                    observation_engines,
                    budget.max_result_bytes,
                    cancelled,
                )
                .await
            };
            !self.result.is_halted()
                && !self.result.cancelled
                && (has_screenings && screening_ok || has_observations && observation_ok)
        }

        fn decode_screening_attempts(
            &mut self,
            turns: Vec<ScreeningAttempt>,
            attempts: Vec<execution::ChildAttempt>,
            engines: Vec<lk_core::execution::AgentEngine>,
            result_limit: usize,
            cancelled: bool,
        ) -> DecodedScreenings {
            let current = self.configuration_is_current();
            if cancelled {
                self.record(
                    "candidate screening",
                    StageState::Cancelled,
                    Some("parallel screening interrupted; unfinished work retained".into()),
                );
            }
            let mut accepted = current && !cancelled;
            let mut replies = Vec::new();
            let mut metrics = Vec::new();
            // Turns whose host could not start: deferred with their profile, never fatal.
            let mut unstarted = false;
            for ((turn, attempt), engine) in turns.into_iter().zip(attempts).zip(engines) {
                let ScreeningAttempt {
                    task_id,
                    decided,
                    candidates,
                    request,
                    metrics: mut step,
                } = turn;
                step.agent_duration_ms = Some(attempt.duration_ms);
                step.host_end = match &attempt.result {
                    Ok(child) => Self::host_end(&step, child.finished_at),
                    Err(_) => None,
                };
                self.result.semantic_duration_ms += attempt.duration_ms;
                let host_end = step.host_end.clone();
                let failed_reply =
                    |failure| crate::commands::work::ScreeningResponse::InvocationFailed {
                        request: request.clone(),
                        failure,
                    };
                let invalid_reply = |bytes: &[u8]| {
                    failed_reply(execution::observation::response_failure(
                        engine,
                        bytes,
                        host_end.clone(),
                    ))
                };
                let decoded = if cancelled || !current {
                    step.outcome = if cancelled {
                        WorkOutcome::Cancelled
                    } else {
                        WorkOutcome::InputsChanged
                    };
                    None
                } else {
                    match attempt.result {
                        Ok(child) if child.success => {
                            step.unanswered =
                                execution::observation::is_answerless(engine, &child.stdout);
                            step.answered = !step.unanswered;
                            match execution::observation::decode_response(
                                engine,
                                &child.stdout,
                                result_limit,
                            ) {
                                Ok(output) => {
                                    step.result_json_bytes = Some(
                                        serde_json::to_vec(&output)
                                            .expect("decoded screening serializes")
                                            .len(),
                                    );
                                    match execution::observation::decode_screening(
                                        output,
                                        &request,
                                        result_limit,
                                    ) {
                                        Ok(output) => Some(crate::commands::work::ScreeningResponse::Decisions {
                                            request: request.clone(),
                                            output,
                                        }),
                                        Err(error) if error.downcast_ref::<crate::commands::work::ScreeningRejection>().is_some() => {
                                            Some(crate::commands::work::ScreeningResponse::Rejected {
                                                request: request.clone(),
                                                rejection: error.downcast_ref::<crate::commands::work::ScreeningRejection>().expect("typed rejection").clone(),
                                            })
                                        }
                                        Err(_) => {
                                            step.host_failure = Some(execution::diagnostic::HostFailure::from_child(engine, execution::diagnostic::Phase::Response, &child));
                                            Some(invalid_reply(&child.stdout))
                                        }
                                    }
                                }
                                Err(error) => {
                                    if error.downcast_ref::<crate::commands::work::state::OutputCapacity>().is_none() {
                                        step.host_failure = Some(execution::diagnostic::HostFailure::from_child(
                                            engine, execution::diagnostic::Phase::Response, &child,
                                        ));
                                    }
                                    Some(invalid_reply(&child.stdout))
                                }
                            }
                        }
                        Ok(child)
                            if child.failed_exit().is_some()
                                || child.termination == execution::ChildTermination::TimedOut =>
                        {
                            step.outcome = WorkOutcome::Failed;
                            step.unanswered = true;
                            step.host_failure =
                                Some(execution::diagnostic::HostFailure::from_child(
                                    engine,
                                    execution::diagnostic::Phase::Invocation,
                                    &child,
                                ));
                            let failure = match child.failed_exit() {
                                Some(code) => {
                                    crate::commands::work::state::InvocationFailure::Exited {
                                        engine,
                                        code,
                                        host: host_end.clone(),
                                    }
                                }
                                None => crate::commands::work::state::InvocationFailure::TimedOut {
                                    engine,
                                    host: host_end.clone(),
                                },
                            };
                            Some(failed_reply(failure))
                        }
                        Ok(child) => {
                            step.outcome = WorkOutcome::Failed;
                            step.host_failure =
                                Some(execution::diagnostic::HostFailure::from_child(
                                    engine,
                                    execution::diagnostic::Phase::Invocation,
                                    &child,
                                ));
                            self.halt("candidate screening", child.detail);
                            None
                        }
                        Err(error) => match self.unstarted_call(&mut step, error) {
                            Ok(reason) => {
                                self.queue_admitted.clear();
                                self.deferred("host unavailable", format!("{task_id}: {reason}"));
                                unstarted = true;
                                None
                            }
                            Err(error) => {
                                step.outcome = WorkOutcome::Failed;
                                self.failure("candidate screening", error);
                                None
                            }
                        },
                    }
                };
                if let Some(output) = decoded {
                    replies.push(ScreeningReply {
                        task_id,
                        output,
                        decided,
                        candidates,
                        metric_index: metrics.len(),
                    });
                } else {
                    accepted = false;
                }
                metrics.push(step);
            }
            DecodedScreenings {
                accepted,
                replies,
                metrics,
                unstarted,
            }
        }
        fn finish_screening_attempts(
            &mut self,
            decoded: DecodedScreenings,
            recorded: Option<(
                miette::Result<Vec<crate::commands::work::ScreeningRecord>>,
                u64,
            )>,
        ) -> bool {
            let DecodedScreenings {
                mut accepted,
                replies,
                mut metrics,
                unstarted,
            } = decoded;
            let mut usable = false;
            let mut failed = false;
            if let Some((recorded, total_duration_ms)) = recorded {
                match recorded {
                    Ok(recorded) => {
                        self.result.work_telemetry.screening_record_duration_ms += recorded
                            .iter()
                            .map(|record| record.duration_ms)
                            .sum::<u64>();
                        let shared_duration_ms = total_duration_ms
                            .saturating_sub(recorded.iter().map(|record| record.duration_ms).sum());
                        for (index, (record, reply)) in
                            recorded.into_iter().zip(&replies).enumerate()
                        {
                            let step = &mut metrics[reply.metric_index];
                            step.revalidation_record_duration_ms = Some(
                                record.duration_ms
                                    + if index == 0 { shared_duration_ms } else { 0 },
                            );
                            match record.result {
                                Ok(
                                    crate::commands::work::ScreeningRecordOutcome::InvocationFailed(
                                        reason,
                                    ),
                                ) => {
                                    step.outcome = WorkOutcome::Failed;
                                    failed = true;
                                    self.queue_admitted.clear();
                                    self.deferred("candidate invocation held", reason);
                                }
                                Ok(crate::commands::work::ScreeningRecordOutcome::Rejected(
                                    reason,
                                )) => {
                                    self.result.work_telemetry.retained_progress.action_receipts +=
                                        1;
                                    step.outcome = WorkOutcome::Rejected;
                                    usable = true;
                                    self.queue_replan = QueueReplan::Invalidate;
                                    self.queue_admitted.clear();
                                    self.record(
                                        "candidate screening",
                                        StageState::Failed,
                                        Some(reason),
                                    );
                                }
                                Ok(crate::commands::work::ScreeningRecordOutcome::Recorded(
                                    recorded,
                                )) => {
                                    step.outcome = WorkOutcome::Recorded;
                                    usable = true;
                                    let decided = reply.decided + recorded;
                                    self.record("candidate screening", StageState::Completed, Some(format!("{decided}/{} candidate decisions recorded; card decisions are not full-body assessments", reply.candidates)));
                                    if decided == reply.candidates {
                                        self.queue_replan.refresh();
                                    }
                                }
                                Err(error) => {
                                    accepted = false;
                                    self.failure("candidate screening", error);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        accepted = false;
                        metrics[replies[0].metric_index].revalidation_record_duration_ms =
                            Some(total_duration_ms);
                        self.failure("candidate screening", error);
                    }
                }
            }
            for mut step in metrics {
                if self.result.cancelled && !matches!(step.outcome, WorkOutcome::Recorded) {
                    step.outcome = WorkOutcome::Cancelled;
                }
                self.record_work_step(step);
            }
            if failed {
                self.replan_after_hold();
            }
            if unstarted {
                self.replan_after_hold();
            }
            accepted && (usable || failed || unstarted)
        }
        async fn record_screening_batch(
            &mut self,
            replies: &[ScreeningReply],
        ) -> Option<(
            miette::Result<Vec<crate::commands::work::ScreeningRecord>>,
            u64,
        )> {
            if replies.is_empty() {
                return None;
            }
            let started = Instant::now();
            let recorded =
                if let Some((session, _)) = self.recording_session("candidate screening").await {
                    Ok(crate::commands::work::record_screenings(
                        &session,
                        replies
                            .iter()
                            .map(|reply| (reply.task_id.as_str(), reply.output.clone()))
                            .collect(),
                        self.supervisor.read_control(),
                    ))
                } else {
                    Err(miette::miette!(
                        "candidate screening could not acquire writer access"
                    ))
                };
            Some((recorded, started.elapsed().as_millis() as u64))
        }

        async fn settle_read_wave(
            &mut self,
            screenings: DecodedScreenings,
            mut observations: DecodedObservations,
        ) -> bool {
            use crate::commands::work::InspectionRecording;
            let replies = std::mem::take(&mut observations.replies);
            let (recorded_screenings, recorded_inspections, inspection_duration_ms) =
                if screenings.replies.is_empty() || replies.is_empty() {
                    (
                        self.record_screening_batch(&screenings.replies).await,
                        InspectionRecording::Recapture(replies),
                        0,
                    )
                } else {
                    let started = Instant::now();
                    if let Some((session, _)) = self.recording_session("queue read receipts").await
                    {
                        let wait_ms = started.elapsed().as_millis() as u64;
                        let pin = crate::commands::ConfigurationPin::from_loaded(
                            self.opts.config.as_ref().expect("resolved config").clone(),
                            self.config_revision.clone(),
                        );
                        let root_pin = self.lease.as_ref().expect("owned workflow").root_pin();
                        let recorded = crate::commands::work::record_read_wave(
                            &session,
                            &self.config,
                            &self.config_revision,
                            screenings
                                .replies
                                .iter()
                                .map(|reply| (reply.task_id.as_str(), reply.output.clone()))
                                .collect(),
                            replies,
                            || {
                                pin.validate()?;
                                root_pin.validate()
                            },
                            self.supervisor.read_control(),
                        );
                        (
                            recorded.screenings.map(|screenings| {
                                (Ok(screenings), recorded.screening_duration_ms + wait_ms)
                            }),
                            recorded.inspections,
                            recorded.inspection_duration_ms,
                        )
                    } else {
                        (
                            Some((
                                Err(miette::miette!(
                                    "queue read receipts could not acquire writer access"
                                )),
                                started.elapsed().as_millis() as u64,
                            )),
                            InspectionRecording::Recapture(replies),
                            0,
                        )
                    }
                };
            let screening_ok = self.finish_screening_attempts(screenings, recorded_screenings);
            let recorded = match recorded_inspections {
                InspectionRecording::Invalidated(error) => {
                    self.configuration_failed = true;
                    self.failure("configuration", error);
                    None
                }
                InspectionRecording::Recorded(recorded) => {
                    Some(self.track_inspection_recording(recorded, inspection_duration_ms))
                }
                InspectionRecording::Recapture(replies) if replies.is_empty() => None,
                InspectionRecording::Recapture(replies) => {
                    self.record_observation_batch(replies).await
                }
            };
            let observation_ok = self.finish_observation_attempts(observations, recorded);
            !self.result.is_halted() && !self.result.cancelled && (screening_ok || observation_ok)
        }

        async fn settle_screening_attempts(
            &mut self,
            turns: Vec<ScreeningAttempt>,
            attempts: Vec<execution::ChildAttempt>,
            engines: Vec<lk_core::execution::AgentEngine>,
            result_limit: usize,
            cancelled: bool,
        ) -> bool {
            let decoded =
                self.decode_screening_attempts(turns, attempts, engines, result_limit, cancelled);
            let recorded = self.record_screening_batch(&decoded.replies).await;
            self.finish_screening_attempts(decoded, recorded)
        }

        async fn queue_dispatches(&mut self) -> miette::Result<QueuePlan> {
            fn flush(
                batch: &mut Vec<(lk_queue::QueueTask, serde_json::Value)>,
                jobs: &mut std::collections::BTreeMap<String, QueueDispatch>,
            ) {
                if let Some((first, _)) = batch.first() {
                    let target = first.target.vault_path.clone();
                    let (tasks, inputs) = std::mem::take(batch).into_iter().unzip();
                    jobs.insert(target, QueueDispatch::WholeInput { tasks, inputs });
                }
            }
            let (session, waited) = self
                .mutation_session("queue planning")
                .await
                .ok_or_else(|| miette::miette!("queue planning did not acquire writer access"))?;
            if waited {
                self.queue_admitted.clear();
            }
            let recovered = crate::commands::queue::rejections::recover_sections_controlled(
                &session,
                &self.config,
                &self.config_revision,
                self.supervisor.read_control(),
            )?;
            self.result.recovered_section_proposals += recovered;
            let mut tasks = crate::commands::queue::current_tasks_controlled(
                session.root(),
                self.supervisor.read_control(),
            )?;
            tasks.sort_by(|left, right| {
                (left.created_at, &left.task_id).cmp(&(right.created_at, &right.task_id))
            });
            let mut owners = std::collections::BTreeMap::new();
            for task in tasks {
                self.supervisor
                    .read_control()
                    .check()
                    .map_err(typed_error)?;
                let owner = lk_vault::page_path(session.root(), Path::new(&task.target.vault_path))
                    .and_then(|path| path.canonicalize())
                    .map_err(|error| miette::miette!("queue target: {error}"))?;
                owners.entry(owner).or_insert(task);
            }
            let mut readiness = owners
                .values()
                .any(|task| task.kind == lk_queue::TaskKind::SynthesizeConcept)
                .then(|| {
                    super::super::queue::readiness::InputReadiness::load_controlled(
                        session.root(),
                        self.supervisor.read_control(),
                    )
                })
                .transpose()?;
            let budget = self
                .config
                .execution
                .effective_work()
                .map_err(typed_error)?;
            let mut deferred = std::collections::BTreeMap::new();
            let mut targets = std::collections::BTreeMap::new();
            let rejected = crate::commands::queue::rejections::blocked_tasks_controlled(
                session.root(),
                &self.config,
                &self.config_revision,
                self.supervisor.read_control(),
            )?;
            let mut needs_application = false;
            for task in owners.into_values() {
                let target = task.target.vault_path.clone();
                if let Some(rejection) =
                    rejected.get(&(task.task_id.clone(), task.cache_hash.clone()))
                {
                    let owner = lk_vault::resolve_page_owner(session.root(), Path::new(&target))
                        .map_err(|error| miette::miette!("rejected queue target: {error}"))?;
                    if owner != Path::new(&rejection.owner) {
                        return Err(miette::miette!("rejected result target owner changed"));
                    }
                    needs_application |= rejection.needs_revalidation;
                    deferred.insert(target, vec![rejection.reason.clone()]);
                    continue;
                }
                if let Some(reason) = self.finalization_replans.deferral(&target) {
                    deferred.insert(target, vec![reason.to_owned()]);
                    continue;
                }
                targets.insert(target, task);
            }
            let mut keys = targets.keys().cloned().collect();
            let mut jobs = std::collections::BTreeMap::new();
            if needs_application || !targets.is_empty() && self.remaining_calls(budget) == 0 {
                return Ok(QueuePlan {
                    jobs,
                    deferred,
                    keys,
                    needs_application,
                });
            }
            let admitted = self
                .queue_admitted
                .remaining_bindings()
                .cloned()
                .collect::<Vec<_>>();
            // A released probe is planned first and alone, ahead of continued and cursor-order
            // work, so it is dispatched before them and never batched with another target.
            let probe = self
                .probe_target(crate::commands::work::host::ProbeScope::Queue)
                .filter(|target| targets.contains_key(target));
            let mut order = probe.iter().cloned().collect::<Vec<_>>();
            let continuation_start = order.len();
            for binding in &admitted {
                if targets.contains_key(&binding.target) {
                    order.push(binding.target.clone());
                } else {
                    self.queue_admitted.clear_binding(binding);
                }
            }
            let continuation_end = order.len();
            order.extend(scheduler::order(
                session.root(),
                scheduler::Stage::Queue,
                &keys,
            )?);
            let independent_limit = self.wave_limit(budget).min(MAX_PREPARED_QUEUE_TARGETS);
            let mut next = 0;
            while next < order.len() {
                self.supervisor
                    .read_control()
                    .check()
                    .map_err(typed_error)?;
                if jobs.contains_key(&order[next]) || !keys.contains(&order[next]) {
                    next += 1;
                    continue;
                }
                let probing = next < continuation_start;
                let continuing = (continuation_start..continuation_end).contains(&next);
                let end = if probing {
                    continuation_start
                } else if continuing {
                    continuation_end
                } else {
                    order.len()
                };
                let batch = is_whole_input_batch_candidate(
                    targets[&order[next]].kind,
                    budget.extraction_context,
                );
                let limit = if !continuing && batch && jobs.is_empty() {
                    MAX_PREPARED_QUEUE_TARGETS
                } else {
                    independent_limit.saturating_sub(jobs.len())
                };
                let count = order[next..end]
                    .iter()
                    .take(limit)
                    .take_while(|target| {
                        !jobs.contains_key(*target)
                            && keys.contains(*target)
                            && is_whole_input_batch_candidate(
                                targets[*target].kind,
                                budget.extraction_context,
                            ) == batch
                    })
                    .count();
                let window = &order[next..next + count];
                next += count;
                let mut materialize = Vec::new();
                for target in window {
                    let task = &targets[target];
                    if task.kind == lk_queue::TaskKind::SynthesizeConcept {
                        let reasons = readiness
                            .as_mut()
                            .expect("synthesis readiness was loaded")
                            .unfinished_synthesis(session.root(), task)?;
                        if !reasons.is_empty() {
                            keys.remove(target);
                            deferred.insert(target.clone(), reasons);
                            continue;
                        }
                    }
                    let needs_discovery = match crate::commands::work::needs_discovery(
                        session.root(),
                        &self.config,
                        &self.config_revision,
                        task,
                    ) {
                        Ok(needs) => needs,
                        Err(error) if crate::commands::work::state::is_input_capacity(&error) => {
                            keys.remove(target);
                            deferred.insert(target.clone(), vec![error.to_string()]);
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if needs_discovery {
                        if let Some(reason) = self.extraction_discovery_hold(task)? {
                            keys.remove(target);
                            deferred.insert(target.clone(), vec![reason]);
                        } else {
                            jobs.insert(
                                target.clone(),
                                QueueDispatch::Discovery { task: task.clone() },
                            );
                        }
                    } else {
                        materialize.push(task.clone());
                    }
                }
                let synthesis_paths = materialize
                    .iter()
                    .filter(|task| task.kind == lk_queue::TaskKind::SynthesizeConcept)
                    .map(|task| task.target.vault_path.clone())
                    .collect::<Vec<_>>();
                let synthesis_inputs = if synthesis_paths.is_empty() {
                    std::collections::BTreeMap::new()
                } else {
                    self.result.queue_planning.synthesis_subjects += synthesis_paths.len();
                    self.result.queue_planning.audit_captures += 1;
                    let pages = lk_graph::scan::scan_vault_controlled(
                        session.root(),
                        self.config.graph.scope.follow_links,
                        self.supervisor.read_control(),
                    )
                    .map_err(typed_error)?;
                    let inputs = lk_graph::audit::load_synthesis_inputs_controlled(
                        session.root(),
                        &self.config.vault.dirs,
                        &pages,
                        &synthesis_paths,
                        self.supervisor.read_control(),
                    )
                    .map_err(typed_error)?;
                    for (target, reason) in inputs.unavailable {
                        keys.remove(&target);
                        materialize.retain(|task| task.target.vault_path != target);
                        deferred.insert(target, vec![reason]);
                    }
                    inputs
                        .candidates
                        .into_iter()
                        .map(|candidate| (candidate.path.clone(), candidate))
                        .collect()
                };
                self.result.queue_planning.prepared_targets += materialize.len();
                let (prepared, captures) = crate::commands::work::prepare_tasks_with_audit(
                    &session,
                    &self.config,
                    &self.config_revision,
                    &materialize,
                    &synthesis_inputs,
                    self.supervisor.read_control(),
                )?;
                self.result.queue_planning.discovery_captures += captures.captures;
                self.result.queue_planning.discovery_io.accumulate(captures);
                let mut prepared = materialize
                    .into_iter()
                    .zip(prepared)
                    .map(|(task, preparation)| {
                        for admission in preparation.admissions {
                            self.result.work_telemetry.record_admission(admission);
                        }
                        (task.target.vault_path.clone(), (task, preparation.work))
                    })
                    .collect::<std::collections::BTreeMap<_, _>>();
                let mut batch = Vec::new();
                for target in window {
                    let Some((task, work)) = prepared.remove(target) else {
                        flush(&mut batch, &mut jobs);
                        continue;
                    };
                    match work {
                        crate::commands::work::TaskWork::Admission { reason } => {
                            flush(&mut batch, &mut jobs);
                            keys.remove(target);
                            deferred.insert(target.clone(), vec![reason]);
                        }
                        crate::commands::work::TaskWork::WholeInput { .. }
                            if execution::finalization::supports(task.kind) =>
                        {
                            flush(&mut batch, &mut jobs);
                            match crate::commands::work::section::prepare_controlled(
                                &session,
                                &self.config,
                                &self.config_revision,
                                &task.task_id,
                                self.supervisor.read_control(),
                            )? {
                                crate::commands::work::section::Preparation::Ready(_) => {
                                    jobs.insert(target.clone(), QueueDispatch::Section { task });
                                }
                                crate::commands::work::section::Preparation::NeedsAction(
                                    reason,
                                ) => {
                                    keys.remove(target);
                                    deferred.insert(target.clone(), vec![reason]);
                                }
                                crate::commands::work::section::Preparation::Capacity {
                                    request_bytes,
                                    max_input_bytes,
                                } => {
                                    keys.remove(target);
                                    deferred.insert(target.clone(), vec![format!("complete section request needs {request_bytes} bytes, above max_input_bytes {max_input_bytes}; retained for explicit capacity adjustment")]);
                                }
                            }
                        }
                        crate::commands::work::TaskWork::WholeInput { native, individual } => {
                            let input =
                                *native.expect("whole-input task has a prepared native input");
                            let compatible = if let Some((previous, _)) = batch.last() {
                                self.config
                                    .execution
                                    .resolve_stage_profile(generation_stage(previous.kind))
                                    .map_err(typed_error)?
                                    .0
                                    == self
                                        .config
                                        .execution
                                        .resolve_stage_profile(generation_stage(task.kind))
                                        .map_err(typed_error)?
                                        .0
                            } else {
                                true
                            };
                            if individual || !compatible {
                                flush(&mut batch, &mut jobs);
                            }
                            let mut inputs = batch
                                .iter()
                                .map(|(_, input)| input.clone())
                                .collect::<Vec<_>>();
                            inputs.push(input.clone());
                            let split = match crate::commands::work::native::selected_request(
                                &self.config, &inputs,
                            ) {
                                Ok(request) => serde_json::to_vec(&request)
                                    .map_err(typed_error)?
                                    .len() > budget.max_input_bytes
                                    || crate::commands::work::native::selected_characters(&inputs)?
                                        > lk_core::semantic::INSPECTION_CHARACTERS.get(),
                                Err(error) if !batch.is_empty() && error.downcast_ref::<crate::commands::work::native::SelectedResponseCapacity>().is_some() => true,
                                Err(error) => return Err(error),
                            };
                            if split {
                                flush(&mut batch, &mut jobs);
                            }
                            batch.push((task, input));
                            if individual {
                                flush(&mut batch, &mut jobs);
                            }
                        }
                        crate::commands::work::TaskWork::Finalization { id } => {
                            flush(&mut batch, &mut jobs);
                            jobs.insert(target.clone(), QueueDispatch::Finalization { id, task });
                        }
                        crate::commands::work::TaskWork::RootFinalization { id } => {
                            flush(&mut batch, &mut jobs);
                            jobs.insert(
                                target.clone(),
                                QueueDispatch::RootFinalization { id, task },
                            );
                        }
                        crate::commands::work::TaskWork::Partitioned { id } => {
                            flush(&mut batch, &mut jobs);
                            if let Some(reason) = self.inspection_deferred.get(&id) {
                                keys.remove(target);
                                deferred.insert(target.clone(), vec![reason.clone()]);
                            } else {
                                jobs.insert(
                                    target.clone(),
                                    QueueDispatch::Partitioned { id, task },
                                );
                            }
                        }
                        crate::commands::work::TaskWork::Source { id } => {
                            flush(&mut batch, &mut jobs);
                            if let Some(reason) = self.inspection_deferred.get(&id) {
                                keys.remove(target);
                                deferred.insert(target.clone(), vec![reason.clone()]);
                            } else {
                                jobs.insert(target.clone(), QueueDispatch::Source { id });
                            }
                        }
                        crate::commands::work::TaskWork::Screening { turn } => {
                            flush(&mut batch, &mut jobs);
                            jobs.insert(
                                target.clone(),
                                QueueDispatch::Screening {
                                    task,
                                    screening_id: turn.screening_id.clone(),
                                    prepared: Some(turn),
                                    prepared_at_call: self.result.semantic_calls,
                                },
                            );
                        }
                    }
                }
                flush(&mut batch, &mut jobs);
                if continuing {
                    for target in window {
                        let binding = admitted
                            .iter()
                            .find(|binding| &binding.target == target)
                            .expect("continued target was admitted");
                        if jobs
                            .get(target)
                            .and_then(|job| job.binding(target))
                            .as_ref()
                            != Some(binding)
                        {
                            self.queue_admitted.clear_binding(binding);
                            jobs.remove(target);
                        }
                    }
                }
                if !jobs.is_empty()
                    && (jobs.len() >= independent_limit
                        || continuing && next == continuation_end
                        || jobs.values().any(|job| !job.is_read_only()))
                {
                    break;
                }
            }
            Ok(QueuePlan {
                jobs,
                deferred,
                keys,
                needs_application,
            })
        }

        async fn expanded_extraction_handoff(
            &mut self,
            tasks: &[lk_queue::QueueTask],
        ) -> miette::Result<bool> {
            let (session, _) = self
                .mutation_session("expanded extraction work")
                .await
                .ok_or_else(|| {
                    miette::miette!("extraction preparation did not acquire writer access")
                })?;
            let root = session.root();
            let pending = crate::commands::queue::PendingTasks::load_controlled(
                root,
                self.supervisor.read_control(),
            )?;
            let mut unfinished = false;
            for task in tasks {
                if crate::commands::queue::tasks_completed_controlled(
                    root,
                    std::slice::from_ref(task),
                    self.supervisor.read_control(),
                )? {
                    continue;
                }
                unfinished = true;
                if task.kind != lk_queue::TaskKind::ExtractConcepts {
                    return Ok(false);
                }
                let current = pending.current_controlled(
                    root,
                    &task.task_id,
                    self.supervisor.read_control(),
                )?;
                if current.kind != task.kind
                    || current.cache_hash != task.cache_hash
                    || current.target != task.target
                    || current.input != task.input
                    || current.created_at != task.created_at
                    || !crate::commands::work::has_current_discovery(root, &current)?
                    || !matches!(
                        crate::commands::work::prepare_task_controlled(
                            &session,
                            &self.config,
                            &self.config_revision,
                            &current,
                            self.supervisor.read_control(),
                        )?
                        .work,
                        crate::commands::work::TaskWork::Partitioned { .. }
                            | crate::commands::work::TaskWork::Finalization { .. }
                            | crate::commands::work::TaskWork::RootFinalization { .. }
                            | crate::commands::work::TaskWork::Source { .. }
                            | crate::commands::work::TaskWork::Screening { .. }
                            | crate::commands::work::TaskWork::Admission { .. }
                    )
                {
                    return Ok(false);
                }
            }
            Ok(unfinished)
        }

        async fn dispatch_queue(&mut self) -> bool {
            self.take_recovery_turn(crate::commands::work::host::ProbeScope::Queue);
            'planning: loop {
                let replan = std::mem::take(&mut self.queue_replan);
                if self.result.is_halted()
                    || self.result.cancelled
                    || !self.configuration_is_current()
                {
                    return false;
                }
                if self.yield_to_waiter() {
                    return false;
                }
                self.queue_turn = None;
                if replan != QueueReplan::Refresh {
                    self.queue_admitted.clear();
                }
                let started_at = Instant::now();
                let acquisition_before = self.result.mutation_access.duration_ms;
                let before = self.result.queue_planning;
                self.result.queue_planning.attempts += 1;
                eprintln!(
                    "{}",
                    serde_json::json!({"type":"lore.queue-planning","state":"started","attempt":self.result.queue_planning.attempts,"elapsed_job_ms":self.started_at.elapsed().as_millis()})
                );
                let planned = self.queue_dispatches().await;
                let acquisition_ms = self.result.mutation_access.duration_ms - acquisition_before;
                self.result.queue_planning.duration_ms +=
                    (started_at.elapsed().as_millis() as u64).saturating_sub(acquisition_ms);
                eprintln!(
                    "{}",
                    serde_json::json!({"type":"lore.queue-planning","state":if planned.is_ok() { "completed" } else { "failed" },"attempt":self.result.queue_planning.attempts,"duration_ms":self.result.queue_planning.duration_ms-before.duration_ms,"prepared_targets":self.result.queue_planning.prepared_targets-before.prepared_targets,"discovery_captures":self.result.queue_planning.discovery_captures-before.discovery_captures,"audit_captures":self.result.queue_planning.audit_captures-before.audit_captures,"elapsed_job_ms":self.started_at.elapsed().as_millis()})
                );
                let plan = match planned {
                    Ok(plan) => plan,
                    Err(_) if self.result.is_halted() || self.result.cancelled => return false,
                    Err(error) => return self.failure("queue planning", error),
                };
                if plan.needs_application {
                    if !self.apply_queue("changed rejected-result validation").await {
                        return false;
                    }
                    continue;
                }
                let mut jobs = plan.jobs;
                if self
                    .is_probe_unplanned(crate::commands::work::host::ProbeScope::Queue, |target| {
                        jobs.contains_key(target)
                    })
                    && self.release_host_probe(
                        crate::commands::work::host::ProbeScope::Queue,
                        ProbeRelease::Unplannable,
                    )
                {
                    continue 'planning;
                }
                if jobs.is_empty() {
                    if !plan.keys.is_empty() && !self.call_budget_available() {
                        return false;
                    }
                    if self.release_host_probe(
                        crate::commands::work::host::ProbeScope::Queue,
                        ProbeRelease::Idle,
                    ) {
                        continue 'planning;
                    }
                    if let Some((target, reasons)) = plan.deferred.first_key_value() {
                        return self.deferred(
                            "queue dependencies",
                            format!(
                                "{} queue target(s) await source publication or explicit action; {target}: {}; unfinished work retained",
                                plan.deferred.len(),
                                reasons.join("; ")
                            ),
                        );
                    }
                    return true;
                }
                loop {
                    if !self.call_budget_available() {
                        return false;
                    }
                    let keys = &plan.keys;
                    let admitted = self
                        .queue_admitted
                        .remaining_bindings()
                        .find(|binding| {
                            jobs.get(&binding.target)
                                .and_then(|job| job.binding(&binding.target))
                                .as_ref()
                                == Some(*binding)
                        })
                        .map(|binding| binding.target.clone());
                    let probing = self
                        .probe_target(crate::commands::work::host::ProbeScope::Queue)
                        .filter(|target| jobs.contains_key(target));
                    let probe_turn = probing.is_some();
                    let selected = match probing.or(admitted) {
                        Some(selected) => selected,
                        None => {
                            self.queue_admitted.clear();
                            match scheduler::order(&self.root, scheduler::Stage::Queue, keys) {
                                Ok(order) if !order.is_empty() => order[0].clone(),
                                Ok(_) => return true,
                                Err(error) => return self.failure("queue scheduling", error),
                            }
                        }
                    };
                    if !jobs.contains_key(&selected) {
                        continue 'planning;
                    }
                    if jobs[&selected].is_read_only() {
                        let probe_keys = std::collections::BTreeSet::from([selected.clone()]);
                        let wave_keys = if probe_turn { &probe_keys } else { keys };
                        if !self.queue_read_wave(&selected, &mut jobs, wave_keys).await {
                            return false;
                        }
                        if self.queue_replan.is_requested()
                            || jobs.values().any(|job| match job {
                                QueueDispatch::Partitioned { id, .. }
                                | QueueDispatch::Source { id } => {
                                    self.inspection_deferred.contains_key(id)
                                }
                                _ => false,
                            })
                        {
                            continue 'planning;
                        }
                        continue;
                    }
                    self.reserved_targets.clear();
                    self.queue_turn = Some(QueueTurn {
                        // A probe's turn is reserved for its target alone, outside the
                        // round-robin order of ordinary targets.
                        keys: if probe_turn {
                            std::collections::BTreeSet::from([selected.clone()])
                        } else {
                            keys.clone()
                        },
                        binding: jobs[&selected].binding(&selected),
                        targets: match &jobs[&selected] {
                            QueueDispatch::WholeInput { tasks, .. } => tasks
                                .iter()
                                .map(|task| task.target.vault_path.clone())
                                .collect(),
                            _ => vec![selected.clone()],
                        },
                    });
                    let mut generation_deferred = false;
                    match &jobs[&selected] {
                        QueueDispatch::Section { task } => {
                            match self.whole_section_step(task).await {
                                WorkStep::Staged => {}
                                WorkStep::Deferred(_) => continue 'planning,
                                WorkStep::Replan => continue 'planning,
                                WorkStep::Failed if self.queue_replan.is_requested() => {
                                    continue 'planning;
                                }
                                WorkStep::Failed => return false,
                                WorkStep::Pending => {
                                    unreachable!("single section has no evidence nodes")
                                }
                            }
                        }
                        QueueDispatch::Discovery { task } => {
                            if !self.discover_extraction(task).await {
                                if self.queue_replan.is_requested() {
                                    continue 'planning;
                                }
                                return false;
                            }
                            continue 'planning;
                        }
                        QueueDispatch::Finalization { id, task } => {
                            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
                            metrics.stages.push(generation_stage(task.kind));
                            metrics.work_id = Some(id.clone());
                            metrics.preparation_duration_ms = Some(0);
                            match self.bounded_finish(id, metrics, true).await {
                                WorkStep::Staged => {}
                                WorkStep::Deferred(_) => continue 'planning,
                                WorkStep::Replan => continue 'planning,
                                WorkStep::Failed if self.queue_replan.is_requested() => {
                                    continue 'planning;
                                }
                                WorkStep::Failed => return false,
                                WorkStep::Pending => {
                                    unreachable!("frontier finalization has no pending inspection")
                                }
                            }
                        }
                        QueueDispatch::RootFinalization { id, task } => {
                            match self.bounded_step(id, generation_stage(task.kind)).await {
                                WorkStep::Pending => continue,
                                WorkStep::Deferred(_) => continue 'planning,
                                WorkStep::Replan => continue 'planning,
                                WorkStep::Failed if self.queue_replan.is_requested() => {
                                    continue 'planning;
                                }
                                WorkStep::Failed => return false,
                                WorkStep::Staged => {}
                            }
                        }
                        QueueDispatch::Partitioned { .. }
                        | QueueDispatch::Source { .. }
                        | QueueDispatch::Screening { .. } => {
                            unreachable!("read-only queue wave handled above")
                        }
                        QueueDispatch::WholeInput { tasks, inputs } => {
                            match self.selected_result_step(tasks, inputs).await {
                                WorkStep::Staged => {}
                                WorkStep::Deferred(_) => generation_deferred = true,
                                WorkStep::Replan => {
                                    generation_deferred = true;
                                }
                                WorkStep::Failed if self.queue_replan.is_requested() => {
                                    continue 'planning;
                                }
                                WorkStep::Failed => return false,
                                WorkStep::Pending => {
                                    unreachable!("whole input has no pending nodes")
                                }
                            }
                        }
                    }
                    self.queue_turn = None;
                    if let Some(binding) = jobs[&selected].binding(&selected) {
                        self.queue_admitted.clear_binding(&binding);
                    }
                    if !self
                        .native("selected queue apply", &["queue", "apply"])
                        .await
                    {
                        return false;
                    }
                    if generation_deferred {
                        if !self
                            .backlinks_sync("queue publication reconciliation")
                            .await
                        {
                            return false;
                        }
                        continue 'planning;
                    }
                    let tasks = match &jobs[&selected] {
                        QueueDispatch::WholeInput { tasks, .. } => tasks.as_slice(),
                        QueueDispatch::Partitioned { task, .. }
                        | QueueDispatch::RootFinalization { task, .. }
                        | QueueDispatch::Finalization { task, .. }
                        | QueueDispatch::Section { task } => std::slice::from_ref(task),
                        QueueDispatch::Discovery { .. } => {
                            unreachable!("discovery restarts planning")
                        }
                        QueueDispatch::Source { .. } | QueueDispatch::Screening { .. } => {
                            unreachable!("preparation restarts planning without publication")
                        }
                    };
                    let mut unfinished = Vec::new();
                    for task in tasks {
                        match crate::commands::queue::tasks_completed_controlled(
                            &self.root,
                            std::slice::from_ref(task),
                            self.supervisor.read_control(),
                        ) {
                            Ok(true) => {
                                self.result
                                    .work_telemetry
                                    .retained_progress
                                    .queue_publication_observed = true
                            }
                            Ok(false) => unfinished.push(task.clone()),
                            Err(error) => return self.failure("selected queue receipt", error),
                        }
                    }
                    let published_sources = self
                        .result
                        .queue_application
                        .latest
                        .as_ref()
                        .is_some_and(|report| {
                            report.groups.iter().any(|group| {
                                matches!(
                                    group.outcome,
                                    crate::commands::queue::application::Outcome::Applied
                                ) && group.identity.as_ref().is_some_and(|identity| {
                                    identity.target.kind != lk_queue::TargetKind::ConceptSynthesis
                                })
                            })
                        });
                    if published_sources
                        && !self
                            .backlinks_sync("queue publication reconciliation")
                            .await
                    {
                        return false;
                    }
                    if !unfinished.is_empty() {
                        let rejected =
                            match crate::commands::queue::rejections::blocked_tasks_controlled(
                                &self.root,
                                &self.config,
                                &self.config_revision,
                                self.supervisor.read_control(),
                            ) {
                                Ok(rejected) => rejected,
                                Err(error) => {
                                    return self.failure("selected queue rejection", error);
                                }
                            };
                        let set_aside = self
                            .result
                            .queue_application
                            .latest
                            .iter()
                            .flat_map(|report| &report.groups)
                            .filter(|group| {
                                matches!(
                                    group.outcome,
                                    crate::commands::queue::application::Outcome::Superseded { .. }
                                        | crate::commands::queue::application::Outcome::Held { .. }
                                )
                            })
                            .filter_map(|group| group.identity.as_ref())
                            .map(|identity| (identity.task_id.clone(), identity.cache_hash.clone()))
                            .collect::<std::collections::BTreeSet<_>>();
                        if unfinished.iter().all(|task| {
                            let key = (task.task_id.clone(), task.cache_hash.clone());
                            rejected.contains_key(&key) || set_aside.contains(&key)
                        }) {
                            continue 'planning;
                        }
                        if matches!(jobs[&selected], QueueDispatch::WholeInput { .. }) {
                            match self.expanded_extraction_handoff(&unfinished).await {
                                Ok(true) => {
                                    self.record("expanded extraction work", StageState::Completed, Some("discovery requires retained source/screening/evidence work or explicit scope admission; replanning unfinished extraction".into()));
                                    continue 'planning;
                                }
                                Ok(false) => {}
                                Err(_) if self.result.is_halted() || self.result.cancelled => {
                                    return false;
                                }
                                Err(error) => {
                                    return self.failure("selected queue receipt", error);
                                }
                            }
                        }
                        return self.failure("selected queue receipt", miette::miette!("selected tasks are not completed against their retained inputs; unfinished work retained"));
                    }
                    break;
                }
            }
        }

        async fn drain(&mut self) -> bool {
            if self.result.is_halted() || self.result.cancelled {
                return false;
            }
            let state = match crate::commands::queue::check::inspect_controlled(
                &self.root,
                self.supervisor.read_control(),
            ) {
                Ok(state) => state,
                Err(error) => return self.failure("queue inspection", error),
            };
            let state = if !state.staged_results.is_empty() || state.pending_mutation {
                if !self
                    .native("pending queue apply", &["queue", "apply"])
                    .await
                {
                    return false;
                }
                match crate::commands::queue::check::inspect_controlled(
                    &self.root,
                    self.supervisor.read_control(),
                ) {
                    Ok(state) => state,
                    Err(error) => return self.failure("queue inspection", error),
                }
            } else {
                state
            };
            match state {
                state if state.current == 0 => self.record(
                    "queue drain",
                    StageState::Skipped,
                    Some("no current tasks".into()),
                ),
                _ if self.config.llm.provider == lk_core::config::LlmProvider::Noop => self
                    .failure(
                        "queue drain",
                        miette::miette!("semantic processing is disabled by llm.provider"),
                    ),
                _ if self.drain_blocked => {
                    self.record(
                        "queue drain",
                        StageState::Skipped,
                        Some("previous consumer failed; pending work retained".into()),
                    );
                    false
                }
                _ => {
                    let completed = self.dispatch_queue().await;
                    self.drain_blocked = self.result.is_halted();
                    completed
                }
            }
        }

        async fn reconcile(&mut self) {
            if self.result.yield_reason.is_some() {
                return;
            }
            self.native("schema", &["schema"]).await;
            self.native("queue apply", &["queue", "apply"]).await;
            self.backlinks_sync("backlinks-sync").await;
            self.retire_queue("queue retirement").await;
            self.drain().await;
            if self.result.yield_reason.is_some() || self.yield_to_waiter() {
                return;
            }
            self.native("queue apply", &["queue", "apply"]).await;
            self.backlinks_sync("backlinks-sync").await;
            self.native("wiki refresh", &["wiki", "refresh"]).await;
            self.graph_lint("graph lint");
            self.native("queue completion", &["queue", "check"]).await;
        }

        /// Queue retirement inside the run, so planning and every later queue read pay for the
        /// work still owed rather than for answered or superseded history. Draining is correct
        /// without it, so a retirement that cannot complete keeps the job incomplete without
        /// halting; rows it cannot classify stay for the run's queue check to report.
        async fn retire_queue(&mut self, name: &str) -> bool {
            if self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return false;
            }
            let Some((session, _)) = self.mutation_session(name).await else {
                return false;
            };
            let retired =
                crate::commands::queue::retire_in_session(&session, self.supervisor.read_control());
            drop(session);
            if !self.configuration_is_current() {
                return false;
            }
            match retired {
                Ok(detail) => self.record(name, StageState::Completed, Some(detail)),
                Err(error) if self.supervisor.is_cancelled() => {
                    self.record(name, StageState::Cancelled, Some(error.to_string()))
                }
                Err(error) => self.record(name, StageState::Failed, Some(error.to_string())),
            }
        }

        async fn inspect_halted_state(&mut self) {
            if !self.result.is_halted() || self.result.cancelled || !self.configuration_is_current()
            {
                return;
            }
            for (name, args) in [
                ("graph lint", vec!["graph", "lint"]),
                ("knowledge reviews", vec!["wiki", "reviews", "--check"]),
            ] {
                if self.result.cancelled || !self.configuration_is_current() {
                    return;
                }
                let mut command = match execution::lore_command(self.opts, &args) {
                    Ok(command) => command,
                    Err(error) => {
                        self.failure(name, error);
                        continue;
                    }
                };
                self.configure_child(&mut command);
                match self
                    .supervisor
                    .run(command, None, false, execution::ChildOutput::Forward)
                    .await
                {
                    Ok(outcome) => {
                        self.record(
                            name,
                            if outcome.cancelled {
                                StageState::Cancelled
                            } else if outcome.success {
                                StageState::Completed
                            } else {
                                StageState::Failed
                            },
                            outcome.detail,
                        );
                    }
                    Err(error) => {
                        self.failure(name, error);
                    }
                }
            }
            if !self.result.cancelled && self.configuration_is_current() {
                self.check_queue("queue completion");
            }
        }

        async fn resume_periods(&mut self) {
            if self.config.synthesis.recovery.is_some()
                && !self
                    .native(
                        "period recovery",
                        &["synthesis", "pending", "--reconcile", "--wait-for-vault"],
                    )
                    .await
            {
                return;
            }
            for stage in ["base", "quarter", "annual"] {
                if self.result.cancelled {
                    return;
                }
                let before = self.failures();
                if !self
                    .native(
                        &format!("period resume {stage}"),
                        &["synthesis", "pending", "--stage", stage, "--wait-for-vault"],
                    )
                    .await
                {
                    return;
                }
                let drained = self.drain().await;
                self.native(&format!("period apply {stage}"), &["queue", "apply"])
                    .await;
                if !drained || self.failures() != before || self.result.cancelled {
                    return;
                }
                if !self
                    .native(
                        &format!("period settle {stage}"),
                        &["synthesis", "pending", "--stage", stage, "--wait-for-vault"],
                    )
                    .await
                {
                    return;
                }
            }
            self.native("period queue completion", &["queue", "check"])
                .await;
            self.native("period navigation", &["wiki", "refresh"]).await;
        }

        fn failures(&self) -> usize {
            self.result
                .stages
                .iter()
                .filter(|stage| matches!(stage.state, StageState::Failed | StageState::Cancelled))
                .count()
        }

        fn inspect_audit_coverage(&mut self) -> miette::Result<lk_graph::audit::AuditCoverage> {
            let pages = lk_graph::scan::scan_vault_controlled(
                &self.root,
                self.config.graph.scope.follow_links,
                self.supervisor.read_control(),
            )
            .map_err(typed_error)?;
            let mut readiness =
                InputAvailability::load_controlled(&self.root, self.supervisor.read_control())?;
            let mut deferred_paths = std::collections::BTreeSet::new();
            let plan = lk_graph::audit::plan_selected_available_partitioned_controlled(
                &self.root,
                &self.config.vault.dirs,
                lk_graph::audit::AuditCorpus::from(&self.config.graph.audit),
                &pages,
                self.config.graph.audit.batch_size,
                &[],
                self.config.graph.audit.max_input_bytes,
                &mut |candidate| {
                    let available = readiness.check(candidate)?;
                    if !available {
                        deferred_paths.insert(candidate.path.clone());
                    }
                    Ok(available)
                },
                self.supervisor.read_control(),
            )
            .map_err(typed_error)?;
            self.audit_deferred_paths.extend(deferred_paths);
            for deferred in readiness.deferred() {
                if let Some(previous) = self
                    .result
                    .audit_deferred
                    .iter_mut()
                    .find(|item| item.path == deferred.path)
                {
                    *previous = deferred.clone();
                } else if self.result.audit_deferred.len()
                    < super::super::audit::MAX_DEFERRED_DETAILS
                {
                    self.result.audit_deferred.push(deferred.clone());
                }
            }
            self.result.audit_deferred_count = self.audit_deferred_paths.len();
            self.count_input_changed(&plan);
            Ok(plan.coverage())
        }

        /// The plan of the exact selection `paths` that is still due. Subjects this run found
        /// removed from the vault leave the selection, and an emptied selection plans nothing,
        /// never the corpus an empty selection names to the planners. An exact plan keeps
        /// current subjects, but one a writer assessed since admission (a manual
        /// `wiki audit-record` during another subject's call) is settled: no audit route sends
        /// it again. A selected subject whose input a writer changed while the selection was
        /// measured stays deferred for the run, one a writer removed leaves the selection, and
        /// their peers are planned again.
        fn audit_plan(&mut self, paths: &[String]) -> miette::Result<lk_graph::audit::AuditPlan> {
            let (plan, readiness, checked_paths, deferred_paths) = loop {
                let selection = paths
                    .iter()
                    .filter(|path| !self.audit_deferrals.removed.contains(*path))
                    .cloned()
                    .collect::<Vec<_>>();
                if selection.is_empty() {
                    return lk_graph::audit::AuditPlan::empty_selection(
                        self.config.graph.audit.batch_size,
                        self.config.graph.audit.max_input_bytes,
                    )
                    .map_err(typed_error);
                }
                let pages = lk_graph::scan::scan_vault_controlled(
                    &self.root,
                    self.config.graph.scope.follow_links,
                    self.supervisor.read_control(),
                )
                .map_err(typed_error)?;
                let mut readiness =
                    InputAvailability::load_controlled(&self.root, self.supervisor.read_control())?;
                let mut checked_paths = selection
                    .iter()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>();
                let mut deferred_paths = std::collections::BTreeSet::new();
                let planned = lk_graph::audit::plan_selected_available_partitioned_controlled(
                    &self.root,
                    &self.config.vault.dirs,
                    lk_graph::audit::AuditCorpus::from(&self.config.graph.audit),
                    &pages,
                    self.config.graph.audit.batch_size,
                    &selection,
                    self.config.graph.audit.max_input_bytes,
                    &mut |candidate| {
                        checked_paths.insert(candidate.path.clone());
                        let available = if let Some(reason) =
                            self.audit_deferrals.deferred.get(&candidate.path)
                        {
                            readiness.defer(candidate, vec![reason.clone()])
                        } else {
                            readiness.check(candidate)?
                        };
                        if !available {
                            deferred_paths.insert(candidate.path.clone());
                        }
                        Ok(available)
                    },
                    self.supervisor.read_control(),
                );
                // Each retry defers or removes another selected subject: a deferred one is not
                // measured and a removed one leaves the selection.
                match planned {
                    Ok(plan) => break (plan, readiness, checked_paths, deferred_paths),
                    Err(lk_graph::GraphError::AuditSubjectInputChanged { subject, input })
                        if !self.audit_deferrals.deferred.contains_key(&subject) =>
                    {
                        let reason = format!("its input {input} changed while it was planned");
                        self.deferred("audit inputs changed", format!("{subject}: {reason}"));
                        self.audit_deferrals.deferred.insert(subject, reason);
                    }
                    Err(lk_graph::GraphError::AuditSubjectRemoved(path))
                        if selection.contains(&path) =>
                    {
                        self.defer_removed_audit_subject(&path, None);
                    }
                    Err(e) => return Err(typed_error(e)),
                }
            };
            self.audit_deferred_paths
                .retain(|path| !checked_paths.contains(path));
            self.result
                .audit_deferred
                .retain(|item| !checked_paths.contains(&item.path));
            self.audit_deferred_paths.extend(deferred_paths);
            self.result
                .audit_deferred
                .extend_from_slice(readiness.deferred());
            self.result
                .audit_deferred
                .truncate(super::super::audit::MAX_DEFERRED_DETAILS);
            self.result.audit_deferred_count = self.audit_deferred_paths.len();
            let mut plan = plan;
            plan.candidates
                .retain(|candidate| candidate.reason != lk_graph::audit::AuditReason::Current);
            Ok(plan)
        }

        /// Records that `path`, a selected audit subject, left the vault after it was planned:
        /// no later plan of this run selects it, and the report counts it with the deferred
        /// subjects, with its `planned` identity when the caller holds one.
        fn defer_removed_audit_subject(
            &mut self,
            path: &str,
            planned: Option<&lk_graph::audit::AuditCandidate>,
        ) {
            self.audit_deferrals.removed.insert(path.to_owned());
            self.deferred("audit subject removed", path);
            match planned {
                Some(candidate) => self.count_deferred_audit(DeferredAudit {
                    kind: candidate.kind,
                    path: candidate.path.clone(),
                    file_revision: candidate.file_revision.clone(),
                    input_revision: candidate.input_revision.clone(),
                    reasons: vec!["the subject left the vault after it was planned".into()],
                }),
                None => {
                    self.audit_deferred_paths.insert(path.to_owned());
                    self.result.audit_deferred_count = self.audit_deferred_paths.len();
                }
            }
        }

        fn audit_screening_limits(
            &self,
        ) -> miette::Result<lk_graph::audit::selection::ScreeningLimits> {
            let budget = self
                .config
                .execution
                .effective_work()
                .map_err(typed_error)?;
            Ok(lk_graph::audit::selection::ScreeningLimits {
                max_input_bytes: budget.max_input_bytes,
                max_result_bytes: budget.max_result_bytes,
                max_candidates: 20,
                max_batches: budget.max_screening_batches.map(|value| value.get()),
            })
        }

        /// The plan after recording a screening of `path`, or after its candidates drifted. A
        /// recorded screening changes only that subject's selection state, so its candidate is
        /// read again from `screened`, the scan the recording checked against (or a fresh one
        /// after drift), and the other planned subjects stand; a deferral (`screened` is `None`)
        /// changes the batch, which is planned again.
        fn replan_screening(
            &mut self,
            paths: &[String],
            mut plan: lk_graph::audit::AuditPlan,
            path: &str,
            screened: Option<Vec<lk_graph::scan::ScannedPage>>,
        ) -> miette::Result<lk_graph::audit::AuditPlan> {
            let Some(pages) = screened else {
                return self.audit_plan(paths);
            };
            let candidate = lk_graph::audit::load_selected_inputs_controlled(
                &self.root,
                &self.config.vault.dirs,
                &pages,
                &[path.to_owned()],
                self.supervisor.read_control(),
            )
            .map_err(typed_error)?
            .pop()
            .ok_or_else(|| {
                miette::miette!("screened audit subject is no longer selectable: {path}")
            })?;
            let slot = plan
                .candidates
                .iter_mut()
                .find(|planned| planned.path == path)
                .ok_or_else(|| miette::miette!("screened audit subject left its plan: {path}"))?;
            *slot = candidate;
            Ok(plan)
        }

        /// Defers `candidate` for the rest of the run because a page its inputs read changed
        /// after it was planned: later plans of the run keep it deferred with `reason`, and the
        /// report counts it with the other deferred subjects.
        fn defer_audit_inputs(
            &mut self,
            candidate: &lk_graph::audit::AuditCandidate,
            reason: String,
        ) {
            self.audit_deferrals
                .deferred
                .insert(candidate.path.clone(), reason.clone());
            self.deferred(
                "audit inputs changed",
                format!("{}: {reason}", candidate.path),
            );
            self.count_deferred_audit(DeferredAudit {
                kind: candidate.kind,
                path: candidate.path.clone(),
                file_revision: candidate.file_revision.clone(),
                input_revision: candidate.input_revision.clone(),
                reasons: vec![reason],
            });
        }

        /// Counts `deferred` with this run's deferred subjects, keeping its detail while the
        /// report has room.
        fn count_deferred_audit(&mut self, deferred: DeferredAudit) {
            if self.audit_deferred_paths.insert(deferred.path.clone())
                && self.result.audit_deferred.len() < super::super::audit::MAX_DEFERRED_DETAILS
            {
                self.result.audit_deferred.push(deferred);
            }
            self.result.audit_deferred_count = self.audit_deferred_paths.len();
        }

        /// Counts the subjects `plan` skipped because an input changed while it read them.
        fn count_input_changed(&mut self, plan: &lk_graph::audit::AuditPlan) {
            for drift in &plan.input_changed {
                self.count_deferred_audit(DeferredAudit {
                    kind: drift.kind,
                    path: drift.path.clone(),
                    file_revision: drift.file_revision.clone(),
                    input_revision: drift.input_revision.clone(),
                    reasons: vec![format!(
                        "its input {} changed while it was planned",
                        drift.input
                    )],
                });
            }
        }

        fn defer_audit_screening(
            &mut self,
            candidate: &lk_graph::audit::AuditCandidate,
            reason: String,
        ) {
            self.audit_deferrals
                .deferred
                .insert(candidate.path.clone(), reason.clone());
            self.deferred(
                "audit candidate selection",
                format!("{}: {reason}", candidate.path),
            );
        }

        async fn screen_audit_inputs(
            &mut self,
            paths: &[String],
            mut plan: lk_graph::audit::AuditPlan,
        ) -> Option<lk_graph::audit::AuditPlan> {
            // A subject removed from the vault leaves the selection (`audit_plan`).
            let limits = match self.audit_screening_limits() {
                Ok(limits) => limits,
                Err(error) => {
                    self.failure("audit screening budget", error);
                    return None;
                }
            };
            let mut drifted = std::collections::BTreeSet::new();
            // Whether only screened subjects were read again since the last complete plan.
            let mut refreshed = false;
            loop {
                let Some(candidate) = plan
                    .candidates
                    .iter()
                    .find(|candidate| {
                        candidate
                            .discovery
                            .as_ref()
                            .and_then(|discovery| discovery.selection.as_ref())
                            .is_some_and(|progress| !progress.is_ready())
                    })
                    .cloned()
                else {
                    if !refreshed {
                        return Some(plan);
                    }
                    // The other planned subjects were read before the screening calls: plan
                    // them again before admission reads them.
                    refreshed = false;
                    plan = match self.audit_plan(paths) {
                        Ok(plan) => plan,
                        Err(error) => {
                            self.failure("audit screening replanning", error);
                            return None;
                        }
                    };
                    continue;
                };
                let packet = match lk_graph::audit::selection::prepare_candidate_packet(
                    &self.root,
                    &self.config.vault.dirs,
                    &candidate,
                    limits,
                    false,
                    self.supervisor.read_control(),
                ) {
                    Ok(packet) => packet,
                    Err(error @ lk_graph::GraphError::AuditScreeningCapacity { .. }) => {
                        self.defer_audit_screening(&candidate, error.to_string());
                        refreshed = false;
                        plan = match self.audit_plan(paths) {
                            Ok(plan) => plan,
                            Err(error) => {
                                self.failure("audit screening replanning", error);
                                return None;
                            }
                        };
                        continue;
                    }
                    // A writer changed this subject's candidates or inputs after it was read, as
                    // while another subject's packet ran: read it again once, and defer it when
                    // they change again.
                    Err(
                        lk_graph::GraphError::AuditScreeningChanged
                        | lk_graph::GraphError::AuditInputChanged(_),
                    ) => {
                        if !drifted.insert(candidate.path.clone()) {
                            self.defer_audit_screening(
                                &candidate,
                                "candidate inputs changed repeatedly before screening".into(),
                            );
                            refreshed = false;
                            plan = match self.audit_plan(paths) {
                                Ok(plan) => plan,
                                Err(error) => {
                                    self.failure("audit screening replanning", error);
                                    return None;
                                }
                            };
                            continue;
                        }
                        let pages = match lk_graph::scan::scan_vault_controlled(
                            &self.root,
                            self.config.graph.scope.follow_links,
                            self.supervisor.read_control(),
                        ) {
                            Ok(pages) => pages,
                            Err(error) => {
                                self.failure("audit screening replanning", typed_error(error));
                                return None;
                            }
                        };
                        refreshed = true;
                        plan = match self.replan_screening(
                            paths,
                            plan,
                            &candidate.path,
                            Some(pages),
                        ) {
                            Ok(plan) => plan,
                            Err(error) => {
                                self.failure("audit screening replanning", error);
                                return None;
                            }
                        };
                        continue;
                    }
                    Err(error) => {
                        self.failure("audit screening preparation", typed_error(error));
                        return None;
                    }
                };
                if let Some(admission) = packet.admission {
                    self.defer_audit_screening(
                        &candidate,
                        format!(
                            "complete discovery needs {} packets, above max_screening_batches {}",
                            admission.required_batches, admission.maximum_batches
                        ),
                    );
                    refreshed = false;
                    plan = match self.audit_plan(paths) {
                        Ok(plan) => plan,
                        Err(error) => {
                            self.failure("audit screening replanning", error);
                            return None;
                        }
                    };
                    continue;
                }
                if let Some(refresh) = packet.refresh {
                    let (session, waited) =
                        self.mutation_session("audit selection refresh").await?;
                    if waited {
                        drop(session);
                        if !drifted.insert(candidate.path.clone()) {
                            self.defer_audit_screening(
                                &candidate,
                                "writer contention repeated during candidate inventory refresh"
                                    .into(),
                            );
                        }
                        refreshed = false;
                        plan = match self.audit_plan(paths) {
                            Ok(plan) => plan,
                            Err(error) => {
                                self.failure("audit selection replanning", error);
                                return None;
                            }
                        };
                        continue;
                    }
                    let result = (|| {
                        let pages = lk_graph::scan::scan_vault_controlled(
                            session.root(),
                            self.config.graph.scope.follow_links,
                            self.supervisor.read_control(),
                        )?;
                        lk_graph::audit::selection::record(
                            &session,
                            &self.config.vault.dirs,
                            &pages,
                            refresh,
                            limits,
                            self.supervisor.read_control(),
                        )
                        .map(|_| pages)
                    })();
                    let screened = match result {
                        Ok(pages) => Some(pages),
                        Err(lk_graph::GraphError::AuditSubjectRemoved(_)) => {
                            self.defer_removed_audit_subject(&candidate.path, Some(&candidate));
                            None
                        }
                        Err(
                            lk_graph::GraphError::AuditScreeningChanged
                            | lk_graph::GraphError::AuditInputChanged(_),
                        ) => {
                            if !drifted.insert(candidate.path.clone()) {
                                self.defer_audit_screening(
                                    &candidate,
                                    "candidate inputs changed repeatedly during inventory refresh"
                                        .into(),
                                );
                            }
                            None
                        }
                        Err(error) => {
                            self.failure("audit selection refresh", typed_error(error));
                            return None;
                        }
                    };
                    drop(session);
                    refreshed = screened.is_some();
                    plan = match self.replan_screening(paths, plan, &candidate.path, screened) {
                        Ok(plan) => plan,
                        Err(error) => {
                            self.failure("audit selection replanning", error);
                            return None;
                        }
                    };
                    continue;
                }
                let Some(request) = packet.request else {
                    self.defer_audit_screening(
                        &candidate,
                        "discovery candidates remain deferred for explicit reassessment".into(),
                    );
                    refreshed = false;
                    plan = match self.audit_plan(paths) {
                        Ok(plan) => plan,
                        Err(error) => {
                            self.failure("audit selection replanning", error);
                            return None;
                        }
                    };
                    continue;
                };
                let packet_revision = packet
                    .request_revision
                    .expect("prepared request has a revision");
                let native =
                    match execution::preparation::screening_request(&request, &packet_revision) {
                        Ok(native) => native,
                        Err(error) => {
                            self.failure("audit screening request", error);
                            return None;
                        }
                    };
                if serde_json::to_vec(&native)
                    .expect("native screening serializes")
                    .len()
                    > limits.max_input_bytes
                {
                    self.defer_audit_screening(
                        &candidate,
                        "complete native screening request exceeds max_input_bytes".into(),
                    );
                    refreshed = false;
                    plan = match self.audit_plan(paths) {
                        Ok(plan) => plan,
                        Err(error) => {
                            self.failure("audit screening replanning", error);
                            return None;
                        }
                    };
                    continue;
                }
                let schema = execution::preparation::screening_schema();
                let binding = match crate::commands::work::preparation::Binding::audit_screening(
                    &request, &native, &schema,
                ) {
                    Ok(binding) => binding,
                    Err(error) => {
                        self.failure("audit screening binding", error);
                        return None;
                    }
                };
                match binding.held(&self.root) {
                    Ok(Some(reason)) => {
                        self.defer_audit_screening(&candidate, reason);
                        refreshed = false;
                        plan = match self.audit_plan(paths) {
                            Ok(plan) => plan,
                            Err(error) => {
                                self.failure("audit screening replanning", error);
                                return None;
                            }
                        };
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.failure("audit screening hold", error);
                        return None;
                    }
                }
                let mut metrics = WorkStepMetrics::new(WorkRole::Screening);
                let response = self.request_result(&native, &schema, &mut metrics).await;
                if let Some(ResultReply::HostUnavailable(reason)) = &response {
                    self.record_work_step(metrics);
                    self.defer_audit_for_host([candidate.path.as_str()], reason);
                    refreshed = false;
                    plan = match self.audit_plan(paths) {
                        Ok(plan) => plan,
                        Err(error) => {
                            self.failure("audit selection replanning", error);
                            return None;
                        }
                    };
                    continue;
                }
                let output = match response {
                    Some(ResultReply::Complete(response)) => {
                        match execution::preparation::screening_output(
                            response.clone(),
                            &request,
                            packet_revision,
                        ) {
                            Ok(execution::preparation::Outcome::Complete(output)) => Ok(output),
                            Ok(execution::preparation::Outcome::NeedsAction(reason)) => Err(
                                crate::commands::work::preparation::Disposition::NeedsAction {
                                    reason,
                                },
                            ),
                            Err(_) => Err(self.invalid_preparation_response(&response, &metrics)),
                        }
                    }
                    Some(ResultReply::InvocationFailed(failure)) => Err(
                        crate::commands::work::preparation::Disposition::invocation_failed(failure),
                    ),
                    Some(ResultReply::HostUnavailable(_)) => {
                        unreachable!("an unsent screening is deferred before its outcome")
                    }
                    None => {
                        self.record_work_step(metrics);
                        return None;
                    }
                };
                let output = match output {
                    Ok(output) => output,
                    Err(disposition) => {
                        self.hold_audit_screening(
                            &candidate.path,
                            &request,
                            &binding,
                            disposition,
                            &mut metrics,
                        )
                        .await;
                        self.record_work_step(metrics);
                        return None;
                    }
                };
                let started = Instant::now();
                let (session, _) = self.mutation_session("audit screening recording").await?;
                let recorded = (|| {
                    let pages = lk_graph::scan::scan_vault_controlled(
                        session.root(),
                        self.config.graph.scope.follow_links,
                        self.supervisor.read_control(),
                    )?;
                    lk_graph::audit::selection::record(
                        &session,
                        &self.config.vault.dirs,
                        &pages,
                        output,
                        limits,
                        self.supervisor.read_control(),
                    )
                    .map(|changed| (changed, pages))
                })();
                drop(session);
                metrics.revalidation_record_duration_ms =
                    Some(started.elapsed().as_millis() as u64);
                let screened = match recorded {
                    Ok((changed, pages)) => {
                        metrics.outcome = WorkOutcome::Recorded;
                        self.result
                            .work_telemetry
                            .retained_progress
                            .screening_packets += usize::from(changed);
                        Some(pages)
                    }
                    Err(lk_graph::GraphError::AuditScreeningChanged) => {
                        metrics.outcome = WorkOutcome::InputsChanged;
                        self.defer_audit_screening(
                            &candidate,
                            "assigned candidate inputs changed; discard the old judgment".into(),
                        );
                        None
                    }
                    Err(lk_graph::GraphError::AuditInputChanged(path)) => {
                        metrics.outcome = WorkOutcome::InputsChanged;
                        self.defer_audit_screening(
                            &candidate,
                            format!("its input {path} changed during its packet; discard the old judgment"),
                        );
                        None
                    }
                    Err(lk_graph::GraphError::AuditSubjectRemoved(_)) => {
                        metrics.outcome = WorkOutcome::InputsChanged;
                        self.defer_removed_audit_subject(&candidate.path, Some(&candidate));
                        None
                    }
                    Err(error) => {
                        self.record_work_step(metrics);
                        self.failure("audit screening recording", typed_error(error));
                        return None;
                    }
                };
                self.record_work_step(metrics);
                refreshed = screened.is_some();
                plan = match self.replan_screening(paths, plan, &candidate.path, screened) {
                    Ok(plan) => plan,
                    Err(error) => {
                        self.failure("audit screening replanning", error);
                        return None;
                    }
                };
            }
        }

        async fn prepare_audit_jobs(
            &mut self,
            candidates: &[lk_graph::audit::AuditCandidate],
        ) -> Option<std::collections::BTreeMap<String, String>> {
            let mut jobs = std::collections::BTreeMap::new();
            for candidate in candidates {
                let (session, _) = self.mutation_session("bounded audit planning").await?;
                match crate::commands::work::prepare_audit_controlled(
                    &session,
                    &self.config,
                    &self.config_revision,
                    candidate,
                    self.supervisor.read_control(),
                ) {
                    Ok(id) => {
                        jobs.insert(candidate.path.clone(), id);
                    }
                    Err(error)
                        if error
                            .downcast_ref::<crate::commands::work::generation::ResultCapacity>()
                            .is_some()
                            || crate::commands::work::state::is_input_capacity(&error) =>
                    {
                        self.deferred(
                            "audit result capacity",
                            format!("{}: {error}", candidate.path),
                        );
                    }
                    // A page a writer edited or removed since the subject was planned.
                    Err(error) if is_semantic_context_changed(&error) => {
                        self.defer_audit_inputs(candidate, error.to_string());
                    }
                    Err(error) => {
                        self.failure("bounded audit planning", error);
                        return None;
                    }
                }
            }
            Some(jobs)
        }

        /// The selected subjects ready for an audit route: planned again (`audit_plan`, which
        /// keeps only due subjects) until each has current discovery, so a recipe an edit
        /// invalidated is discovered again before either route.
        async fn replan_audit(&mut self, paths: &[String]) -> Option<lk_graph::audit::AuditPlan> {
            let mut plan = match self.audit_plan(paths) {
                Ok(plan) => plan,
                Err(error) => {
                    self.failure("audit replanning", error);
                    return None;
                }
            };
            loop {
                let missing = plan
                    .candidates
                    .iter()
                    .filter(|candidate| candidate.discovery.is_none())
                    .map(|candidate| candidate.path.clone())
                    .collect::<Vec<_>>();
                if missing.is_empty() {
                    break;
                }
                if !self.discover_audit(&missing).await {
                    return None;
                }
                plan = match self.audit_plan(paths) {
                    Ok(plan) => plan,
                    Err(error) => {
                        self.failure("audit discovery replanning", error);
                        return None;
                    }
                };
            }
            self.screen_audit_inputs(paths, plan).await
        }

        async fn admit_audit(&mut self) -> miette::Result<lk_graph::audit::AuditPlan> {
            let (session, _) = self
                .mutation_session("audit admission")
                .await
                .ok_or_else(|| miette::miette!("audit admission did not acquire writer access"))?;
            let pages = lk_graph::scan::scan_vault_controlled(
                &self.root,
                self.config.graph.scope.follow_links,
                self.supervisor.read_control(),
            )
            .map_err(typed_error)?;
            let _queue =
                super::super::queue::claim_writer(&self.root.join(".lorekeeper/queue"), false)?;
            let focus = self.probe_target(crate::commands::work::host::ProbeScope::Audit);
            let mut readiness =
                InputAvailability::load_controlled(&self.root, self.supervisor.read_control())?;
            let mut processing_deferred = Vec::new();
            let mut input_deferred_paths = std::collections::BTreeSet::new();
            let mut processing_deferred_paths = std::collections::BTreeSet::new();
            let plan = lk_graph::audit::admit_available_partitioned_controlled(
                &session,
                &self.config.vault.dirs,
                lk_graph::audit::AuditCorpus::from(&self.config.graph.audit),
                &pages,
                self.config.graph.audit.batch_size,
                self.config.graph.audit.max_input_bytes,
                &mut |candidate| {
                    // A released probe's subject goes first: this batch admits it alone.
                    if focus.as_ref().is_some_and(|focus| *focus != candidate.path) {
                        return Ok(false);
                    }
                    // A subject an earlier batch of this invocation deferred, or found removed
                    // before a page reappeared at its path, waits for a later invocation.
                    let available = if let Some(reason) =
                        self.audit_deferrals.deferred.get(&candidate.path)
                    {
                        readiness.defer(candidate, vec![reason.clone()])
                    } else if self.audit_deferrals.removed.contains(&candidate.path) {
                        readiness.defer(
                            candidate,
                            vec!["the subject left the vault earlier in this invocation".into()],
                        )
                    } else {
                        readiness.check(candidate)?
                    };
                    if !available {
                        input_deferred_paths.insert(candidate.path.clone());
                    }
                    Ok(available)
                },
                &mut |candidate| {
                    self.supervisor
                        .read_control()
                        .check()
                        .map_err(lk_graph::GraphError::from)?;
                    let reason = match crate::commands::work::audit_processing_hold_controlled(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        candidate,
                        self.supervisor.read_control(),
                    ) {
                        Ok(hold) => {
                            hold.map(crate::commands::work::AuditProcessingHold::into_reason)
                        }
                        // A page a writer edited after admission derived the subject: it waits
                        // for a later run.
                        Err(error)
                            if error
                                .downcast_ref::<crate::commands::work::SourceChanged>()
                                .is_some() =>
                        {
                            Some(format!("its inputs changed during admission: {error}"))
                        }
                        Err(error) => return Err(lk_graph::GraphError::Io(error.to_string())),
                    };
                    self.supervisor
                        .read_control()
                        .check()
                        .map_err(lk_graph::GraphError::from)?;
                    let Some(reason) = reason else {
                        return Ok(true);
                    };
                    processing_deferred_paths.insert(candidate.path.clone());
                    if processing_deferred.len() < super::super::audit::MAX_DEFERRED_DETAILS {
                        processing_deferred.push(DeferredAudit {
                            kind: candidate.kind,
                            path: candidate.path.clone(),
                            file_revision: candidate.file_revision.clone(),
                            input_revision: candidate.input_revision.clone(),
                            reasons: vec![reason],
                        });
                    }
                    Ok(false)
                },
                |candidates, mutation| {
                    let mut selected = InputAvailability::load_controlled(
                        &self.root,
                        self.supervisor.read_control(),
                    )
                    .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
                    for candidate in candidates {
                        selected
                            .require_ready(candidate)
                            .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
                    }
                    selected
                        .guard(mutation)
                        .map_err(|error| lk_graph::GraphError::Io(error.to_string()))
                },
                self.supervisor.read_control(),
            )
            .map_err(typed_error)?;
            self.result.audit_deferred = readiness.deferred().to_vec();
            self.result.audit_deferred.extend(processing_deferred);
            self.result
                .audit_deferred
                .truncate(super::super::audit::MAX_DEFERRED_DETAILS);
            self.audit_deferred_paths = input_deferred_paths;
            self.audit_deferred_paths.extend(processing_deferred_paths);
            self.result.audit_deferred_count = self.audit_deferred_paths.len();
            self.count_input_changed(&plan);
            Ok(plan)
        }

        fn verify_audit(
            &mut self,
            batch: &[lk_graph::audit::AuditCandidate],
        ) -> miette::Result<lk_graph::audit::AuditVerification> {
            let pages = lk_graph::scan::scan_vault_controlled(
                &self.root,
                self.config.graph.scope.follow_links,
                self.supervisor.read_control(),
            )
            .map_err(typed_error)?;
            let workflow = self
                .lease
                .as_ref()
                .ok_or_else(|| miette::miette!("audit has no workflow lease"))?;
            let mut readiness =
                InputAvailability::load_controlled(&self.root, self.supervisor.read_control())?;
            let mut checked = std::collections::BTreeMap::new();
            let verified = lk_graph::audit::verify_batch_with_readiness_controlled(
                &self.root,
                &self.config.vault.dirs,
                lk_graph::audit::AuditCorpus::from(&self.config.graph.audit),
                &pages,
                batch,
                workflow.id(),
                |candidate| {
                    checked.insert(candidate.path.clone(), candidate.clone());
                    readiness.check(candidate)
                },
                self.supervisor.read_control(),
            )
            .map_err(typed_error)?;
            for path in &verified.pending {
                if !readiness
                    .deferred()
                    .iter()
                    .any(|deferred| &deferred.path == path)
                {
                    readiness.defer(&checked[path], vec!["audit inputs changed after assessment; retained for a fresh assessment".into()]);
                }
                self.audit_deferred_paths.insert(path.clone());
            }
            self.result
                .audit_deferred
                .extend_from_slice(readiness.deferred());
            self.result
                .audit_deferred
                .truncate(super::super::audit::MAX_DEFERRED_DETAILS);
            self.result.audit_deferred_count = self.audit_deferred_paths.len();
            Ok(verified)
        }

        fn inspect_review_health(&mut self) -> bool {
            if self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            self.result.review_health = None;
            let reports = match lk_vault::list_reviews_controlled(
                &self.root,
                &self.config.vault.dirs,
                self.supervisor.read_control(),
            ) {
                Ok(reports) => reports,
                Err(error) => return self.failure("knowledge review health", typed_error(error)),
            };
            if !self.configuration_is_current() {
                return false;
            }
            let health = lk_vault::ReviewHealth::from_reports(&reports);
            let detail = format!(
                "open={}, resolved={}, needs_reassessment={}, invalid={}; inspect `lore wiki reviews` for details",
                health.open, health.resolved, health.needs_reassessment, health.invalid,
            );
            let valid = health.invalid == 0;
            self.result.review_health = Some(health);
            if valid {
                self.record(
                    "knowledge review health",
                    StageState::Completed,
                    Some(detail),
                )
            } else {
                self.halt("knowledge review health", Some(detail))
            }
        }

        fn check_reviews(&mut self) -> bool {
            if self.result.yield_reason.is_some() {
                return false;
            }
            if self.result.is_halted() || !self.inspect_review_health() {
                return false;
            }
            let pending = self
                .result
                .review_health
                .as_ref()
                .expect("inspected review health")
                .needs_reassessment;
            if pending > 0 {
                self.deferred(
                    "knowledge reviews",
                    format!("{pending} reviews require reassessment; inspect `lore wiki reviews`"),
                )
            } else {
                self.record("knowledge reviews", StageState::Completed, None)
            }
        }

        async fn knowledge(&mut self) {
            self.reconcile_audit_inputs().await;
            if self.result.is_halted() || self.result.cancelled {
                return;
            }
            // The audit is semantic work: under `llm.provider: noop` it is not admitted, and
            // the deterministic reconciliation and checks still run.
            if self.config.invokes_semantic_hosts() {
                while self.audit().await {}
            } else {
                self.record(
                    "audit",
                    StageState::Skipped,
                    Some("semantic execution is disabled by llm.provider".into()),
                );
            }
            let yielded = self.result.yield_reason.is_some() || self.yield_to_waiter();
            if !yielded {
                self.reconcile().await;
                if self.result.yield_reason.is_none() && !self.inspect_review_health() {
                    return;
                }
            }
            if let Some(batch) = self.audit_batch.take() {
                match self.verify_audit(&batch) {
                    Ok(verified) => {
                        self.result.audit = Some(verified.completed);
                        if let Some(progress) = &mut self.result.audit_progress {
                            progress.after = Some(verified.coverage);
                        }
                        if self.result.audit_deferred_count > 0 {
                            self.deferred("audit completion", format!("{} checked audit subjects require fresh assessment or completed inputs", self.result.audit_deferred_count));
                        } else {
                            self.record("audit completion", StageState::Completed, None);
                        }
                    }
                    Err(error) => {
                        self.failure("audit completion", error);
                    }
                }
            } else if self.result.yield_reason.is_none()
                && self.result.audit.as_ref().is_some_and(Vec::is_empty)
            {
                match self.inspect_audit_coverage() {
                    Ok(coverage) => {
                        if let Some(progress) = &mut self.result.audit_progress {
                            progress.after = Some(coverage);
                        }
                        if self.result.audit_deferred_count > 0 {
                            self.deferred(
                                "audit final dependencies",
                                format!(
                                    "{} checked audit subjects have unfinished inputs; retained for later assessment",
                                    self.result.audit_deferred_count,
                                ),
                            );
                        }
                    }
                    Err(error) => {
                        self.failure("audit final coverage", error);
                    }
                }
            }
        }

        async fn reconcile_audit_inputs(&mut self) {
            if self.native("schema", &["schema"]).await
                && self.native("queue apply", &["queue", "apply"]).await
                && self.backlinks_sync("backlinks-sync").await
                && self.native("wiki refresh", &["wiki", "refresh"]).await
            {
                self.graph_lint("graph lint");
            }
        }

        /// Plans and runs one audit batch. Returns whether a released probe asks for planning
        /// again.
        async fn audit(&mut self) -> bool {
            if !self.inspect_review_health() {
                return false;
            }
            self.take_recovery_turn(crate::commands::work::host::ProbeScope::Audit);
            let mut plan = match self.admit_audit().await {
                Ok(plan) => plan,
                Err(_) if self.result.is_halted() || self.result.cancelled => return false,
                Err(error) => {
                    self.failure("audit planning", error);
                    return false;
                }
            };
            // Admission focuses on a released probe's subject; one the current audit scope,
            // filters or inputs do not admit is withdrawn, and admission runs again for the
            // next candidate or, with none left, for ordinary subjects.
            if self.is_probe_unplanned(crate::commands::work::host::ProbeScope::Audit, |target| {
                plan.candidates
                    .iter()
                    .any(|candidate| candidate.path == target)
            }) {
                self.release_host_probe(
                    crate::commands::work::host::ProbeScope::Audit,
                    ProbeRelease::Unplannable,
                );
                return !self.result.is_halted();
            }
            self.result.audit_progress = Some(AuditProgress {
                before: plan.coverage(),
                after: None,
            });
            self.result.audit_blocked = plan.blocked;
            self.result.audit_blocked_count = plan.blocked_count;
            if plan.blocked_count > 0 {
                self.record(
                    "audit input bound",
                    if plan.candidates.is_empty() {
                        StageState::Failed
                    } else {
                        StageState::Skipped
                    },
                    Some(format!(
                        "{} encountered concepts exceed the complete input bound and remain pending; {} details reported",
                        plan.blocked_count,
                        self.result.audit_blocked.len(),
                    )),
                );
            }
            if plan.candidates.is_empty() {
                if self.result.audit_deferred_count > 0 {
                    if self.release_host_probe(
                        crate::commands::work::host::ProbeScope::Audit,
                        ProbeRelease::Idle,
                    ) {
                        return true;
                    }
                    self.deferred("audit dependencies", "pending subjects have unfinished inputs or require processing action; retained for later assessment");
                    return false;
                }
                self.result.audit = Some(Vec::new());
                if plan.blocked_count > 0 {
                    return false;
                }
                if let Some(progress) = &mut self.result.audit_progress {
                    progress.after = Some(progress.before.clone());
                }
                self.record(
                    "knowledge audit",
                    StageState::Skipped,
                    Some("no audit candidates".into()),
                );
                return false;
            }
            let paths = plan
                .candidates
                .iter()
                .map(|candidate| candidate.path.clone())
                .collect::<Vec<_>>();
            let Some(fresh) = self.replan_audit(&paths).await else {
                return false;
            };
            plan = fresh;
            let selected_deferred = paths
                .iter()
                .filter(|path| self.audit_deferred_paths.contains(*path))
                .count();
            if selected_deferred > 0 {
                self.deferred(
                    "audit discovery dependencies",
                    format!("{selected_deferred} selected subjects have unfinished inputs or deferred candidate decisions; retained for later assessment"),
                );
            }
            if plan.candidates.is_empty() {
                if selected_deferred == 0 {
                    self.result.audit = Some(Vec::new());
                    self.record(
                        "knowledge audit",
                        StageState::Skipped,
                        Some("selected subjects are already current".into()),
                    );
                }
                return false;
            }
            // The route reads the combined input of the subjects it keeps: a subject whose
            // inputs drifted, during request planning or while its bounded Work was prepared,
            // is deferred, and the rest are planned, measured and routed again without it.
            let (bounded, mut prepared) = loop {
                let bounded = loop {
                    let mut bounded = plan.input.ensure_ready().is_err();
                    // Both routes below capture each subject again, so a wait needs no replan here.
                    let Some((session, _)) = self.mutation_session("audit request planning").await
                    else {
                        return false;
                    };
                    let mut drifted = std::collections::BTreeSet::new();
                    for candidate in &plan.candidates {
                        match crate::commands::work::audit_needs_work_controlled(
                            &session,
                            &self.config,
                            &self.config_revision,
                            candidate,
                            self.supervisor.read_control(),
                        ) {
                            Ok(required) => bounded |= required,
                            Err(error) if is_semantic_context_changed(&error) => {
                                self.defer_audit_inputs(candidate, error.to_string());
                                drifted.insert(candidate.path.clone());
                            }
                            Err(error) => {
                                self.failure("audit request planning", error);
                                return false;
                            }
                        }
                    }
                    drop(session);
                    if drifted.is_empty() {
                        break bounded;
                    }
                    let remaining = plan
                        .candidates
                        .iter()
                        .filter(|candidate| !drifted.contains(&candidate.path))
                        .map(|candidate| candidate.path.clone())
                        .collect::<Vec<_>>();
                    if remaining.is_empty() {
                        return false;
                    }
                    let Some(fresh) = self.replan_audit(&remaining).await else {
                        return false;
                    };
                    plan = fresh;
                    if plan.candidates.is_empty() {
                        return false;
                    }
                };
                if !bounded {
                    break (false, None);
                }
                let Some(jobs) = self.prepare_audit_jobs(&plan.candidates).await else {
                    return false;
                };
                let remaining = plan
                    .candidates
                    .iter()
                    .filter(|candidate| jobs.contains_key(&candidate.path))
                    .map(|candidate| candidate.path.clone())
                    .collect::<Vec<_>>();
                if remaining.len() == plan.candidates.len() {
                    break (true, Some(jobs));
                }
                if remaining.is_empty() {
                    return false;
                }
                let Some(fresh) = self.replan_audit(&remaining).await else {
                    return false;
                };
                plan = fresh;
                if plan.candidates.is_empty() {
                    return false;
                }
            };
            let mut deferred_subjects = std::collections::BTreeSet::new();
            if bounded {
                let bindings = plan
                    .candidates
                    .iter()
                    .map(|candidate| {
                        (
                            candidate.path.clone(),
                            self.audit_replans.bind(&candidate.path),
                        )
                    })
                    .collect::<std::collections::BTreeMap<_, _>>();
                let mut originals = plan
                    .candidates
                    .iter()
                    .map(|candidate| (candidate.path.clone(), vec![candidate.clone()]))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let mut jobs = prepared
                    .take()
                    .expect("the bounded route prepared its Work");
                while !jobs.is_empty() {
                    jobs.retain(|path, work| {
                        if self.inspection_deferred.contains_key(work) {
                            deferred_subjects.insert(path.clone());
                            false
                        } else {
                            true
                        }
                    });
                    if jobs.is_empty() {
                        break;
                    }
                    if !self.call_budget_available() {
                        return false;
                    }
                    // A released probe's subject takes its turn alone, outside the round-robin
                    // order of the other subjects.
                    let keys = match self
                        .probe_target(crate::commands::work::host::ProbeScope::Audit)
                        .filter(|target| jobs.contains_key(target))
                    {
                        Some(target) => std::collections::BTreeSet::from([target]),
                        None => jobs.keys().cloned().collect(),
                    };
                    // The turn is chosen without writing the cursor; it is reserved only once
                    // its calls are prepared and admitted.
                    // A wave reply that found a subject's Work inputs changed replans that
                    // subject, spending its allowance once, as its own step would.
                    let wave_replan = loop {
                        match self.audit_wave_replans.pop_first() {
                            Some(subject) if jobs.contains_key(&subject) => break Some(subject),
                            Some(_) => continue,
                            None => break None,
                        }
                    };
                    let (selected, step) = if let Some(subject) = wave_replan {
                        (subject, WorkStep::Replan)
                    } else {
                        let selected =
                            match scheduler::order(&self.root, scheduler::Stage::Audit, &keys) {
                                Ok(order) => match order.into_iter().next() {
                                    Some(selected) => selected,
                                    None => break,
                                },
                                Err(error) => {
                                    self.failure("audit scheduling", error);
                                    return false;
                                }
                            };
                        if let Some(completed) =
                            self.bounded_audit_wave(&selected, &jobs, &keys).await
                        {
                            if !completed {
                                return false;
                            }
                            continue;
                        }
                        self.audit_turn = Some((keys.clone(), selected.clone()));
                        let step = self
                            .bounded_step(&jobs[&selected], ExecutionStage::Audit)
                            .await;
                        self.audit_turn = None;
                        (selected, step)
                    };
                    match step {
                        WorkStep::Pending => continue,
                        WorkStep::Deferred(reason) => {
                            self.deferred("bounded audit action", reason);
                            deferred_subjects.insert(selected.clone());
                            jobs.remove(&selected);
                            continue;
                        }
                        WorkStep::Failed => return false,
                        WorkStep::Replan => {
                            let mut exhausted = Vec::new();
                            originals
                                .get_mut(&selected)
                                .expect("selected audit has original slots")
                                .retain(|original| {
                                    self.audit_replans.try_spend(&bindings[&original.path]) || {
                                        exhausted.push(original.clone());
                                        false
                                    }
                                });
                            for original in exhausted {
                                deferred_subjects.insert(original.path.clone());
                                self.defer_audit_inputs(&original, AUDIT_UNSTABLE.into());
                            }
                            if originals[&selected].is_empty() {
                                deferred_subjects.insert(selected.clone());
                                jobs.remove(&selected);
                                continue;
                            }
                            let mut pending = jobs
                                .keys()
                                .flat_map(|path| originals[path].iter().cloned())
                                .collect::<Vec<_>>();
                            // A subject a writer removed outside the workflow is deferred and
                            // leaves the batch; its peers are resolved again without it.
                            let current = loop {
                                let error = match self.current_audit_subjects(&pending) {
                                    Ok(current) => break current,
                                    Err(error) => error,
                                };
                                let Some(lk_graph::GraphError::AuditSubjectRemoved(path)) =
                                    error.downcast_ref::<lk_graph::GraphError>()
                                else {
                                    self.failure("bounded audit fresh scope", error);
                                    return false;
                                };
                                let Some(removed) = pending
                                    .iter()
                                    .find(|original| original.path == *path)
                                    .cloned()
                                else {
                                    self.failure("bounded audit fresh scope", error);
                                    return false;
                                };
                                self.defer_audit_inputs(&removed, error.to_string());
                                deferred_subjects.insert(removed.path.clone());
                                pending.retain(|original| original.path != removed.path);
                            };
                            if pending.is_empty() {
                                jobs.clear();
                                continue;
                            }
                            let mut current_originals = std::collections::BTreeMap::<
                                String,
                                Vec<lk_graph::audit::AuditCandidate>,
                            >::new();
                            for (original, candidate) in pending.into_iter().zip(current) {
                                current_originals
                                    .entry(candidate.path)
                                    .or_default()
                                    .push(original);
                            }
                            let paths = current_originals.keys().cloned().collect::<Vec<_>>();
                            let Some(fresh) = self.replan_audit(&paths).await else {
                                return false;
                            };
                            let Some(fresh_jobs) = self.prepare_audit_jobs(&fresh.candidates).await
                            else {
                                return false;
                            };
                            for (path, subjects) in &current_originals {
                                if !fresh_jobs.contains_key(path) {
                                    deferred_subjects.extend(
                                        subjects.iter().map(|subject| subject.path.clone()),
                                    );
                                }
                            }
                            originals = current_originals;
                            jobs = fresh_jobs;
                            self.record("bounded audit replanning", StageState::Skipped, Some("captured fresh selected inputs; prior Work remains unchanged and the current run budget is retained".into()));
                            continue;
                        }
                        WorkStep::Staged => {}
                    }
                    let assessment = self
                        .root
                        .join(".lorekeeper/work")
                        .join(&jobs[&selected])
                        .join("assessment.json");
                    let assessment = match assessment.to_str() {
                        Some(path) => path.to_owned(),
                        None => {
                            self.failure(
                                "bounded audit publication",
                                miette::miette!("assessment path is not UTF-8"),
                            );
                            return false;
                        }
                    };
                    if !self
                        .native(
                            "bounded audit publication",
                            &["wiki", "audit-record", "--file", &assessment],
                        )
                        .await
                    {
                        return false;
                    }
                    self.audit_batch
                        .get_or_insert_with(Vec::new)
                        .extend(originals[&selected].iter().cloned());
                    jobs.remove(&selected);
                }
            } else {
                self.whole_audit(&plan.candidates).await;
                return false;
            }
            if !deferred_subjects.is_empty() {
                self.deferred("audit inspection dependencies", format!("{} selected audit subjects require explicit processing action; no assessment was recorded for them", deferred_subjects.len()));
            }
            if self.audit_batch.as_ref().is_none_or(Vec::is_empty) {
                self.result.audit = Some(Vec::new());
            }
            false
        }
    }

    struct Campaign {
        supervisor: execution::Supervisor,
        host_output: execution::ChildOutput,
        root: Option<workflow::VaultRootPin>,
        automation: Option<automation::Control>,
        audit_replans: AuditReplans,
        finalization_replans: FinalizationReplans,
        audit_deferrals: AuditDeferrals,
    }

    pub(super) async fn run(
        opts: &GlobalOptions,
        installation: &lk_dist::Installation,
        options: RunOptions,
    ) -> miette::Result<()> {
        let RunOptions {
            operation,
            profile,
            batches,
            wait,
            json,
            scheduled,
            unattended,
            resume,
        } = options;
        let batches = batches.get();
        let started_at = Instant::now();
        if batches != 1 && matches!(operation, Operation::Daily) {
            return Err(miette::miette!(
                "multiple batches are only supported for process and knowledge jobs"
            ));
        }
        let config_path = crate::commands::find_config(opts)?;
        let (mut config, config_revision) =
            crate::commands::load_config_with_revision(&config_path)?;
        let explicit = profile
            .as_deref()
            .or(config.execution.selected_profile())
            .map(str::to_owned);
        if explicit.is_some()
            || config.execution.default_profile.is_some()
            || config
                .execution
                .operations
                .contains_key(&operation.semantic())
        {
            let name = config
                .execution
                .resolve_profile(operation.semantic(), explicit.as_deref())
                .map_err(typed_error)?
                .0
                .to_owned();
            config
                .execution
                .select_profile(&name)
                .map_err(typed_error)?;
        }
        let options = GlobalOptions {
            config: Some(config_path),
            template_dir: opts
                .template_dir
                .as_ref()
                .map(std::path::absolute)
                .transpose()
                .map_err(|e| miette::miette!("template path: {e}"))?,
        };
        let automation = if scheduled {
            let policy = config
                .execution
                .automation
                .clone()
                .ok_or_else(|| miette::miette!("--scheduled requires execution.automation"))?;
            Some(automation::Control::new(
                policy,
                operation.semantic(),
                resume,
                config
                    .execution
                    .effective_work()
                    .map_err(typed_error)?
                    .max_calls,
            )?)
        } else {
            None
        };
        let mut campaign = Campaign {
            supervisor: execution::Supervisor::new()?,
            host_output: if scheduled || unattended {
                execution::ChildOutput::Discard
            } else {
                execution::ChildOutput::Forward
            },
            root: None,
            automation,
            audit_replans: AuditReplans::default(),
            finalization_replans: FinalizationReplans::default(),
            audit_deferrals: AuditDeferrals::default(),
        };
        let mut outcomes = Vec::new();
        let mut stop_reason = CampaignStop::BatchLimit;
        let batch_limit = if scheduled && !matches!(operation, Operation::Daily) {
            usize::MAX
        } else {
            batches
        };
        for _ in 0..batch_limit {
            let result = execute(
                &options,
                installation,
                (config.clone(), config_revision.clone()),
                operation,
                wait,
                &mut campaign,
            )
            .await;
            stop_reason = result.campaign_stop();
            outcomes.push(result);
            if !matches!(
                stop_reason,
                CampaignStop::BatchLimit | CampaignStop::Yielded
            ) {
                break;
            }
        }
        let ok = if matches!(operation, Operation::Process) && batch_limit > 1 {
            stop_reason == CampaignStop::Settled
        } else {
            outcomes
                .iter()
                .all(|result| result.ok || result.is_successful_yield())
        };
        if json {
            let value = if batch_limit == 1 {
                serde_json::to_value(&outcomes[0])
            } else {
                Ok(serde_json::json!({
                    "operation":operation,"ok":ok,"requested_batches":(!scheduled).then_some(batches),
                    "stop_reason":stop_reason,"runs":outcomes,
                    "duration_ms":started_at.elapsed().as_millis() as u64,
                }))
            }
            .map_err(typed_error)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(typed_error)?
            );
        }
        if !ok {
            return Err(miette::miette!(
                "job incomplete; inspect failed stages and retained work"
            ));
        }
        Ok(())
    }

    async fn execute(
        options: &GlobalOptions,
        installation: &lk_dist::Installation,
        configuration: (Config, String),
        operation: Operation,
        wait: bool,
        campaign: &mut Campaign,
    ) -> JobResult {
        let Campaign {
            supervisor,
            host_output,
            root: campaign_root,
            automation,
            audit_replans,
            finalization_replans,
            audit_deferrals,
        } = campaign;
        let (config, config_revision) = configuration;
        let board = config
            .personal
            .as_ref()
            .and_then(|personal| personal.tasks.as_ref())
            .is_some();
        let mut runner = Runner {
            installation,
            root: lk_core::fs::canonical_prefix(&config.vault.root_path()),
            queue_admitted: scheduler::AdmittedTurns::new(
                config
                    .execution
                    .effective_work()
                    .expect("validated execution limits")
                    .max_consecutive_calls,
            ),
            config,
            config_revision,
            configuration_failed: false,
            opts: options,
            supervisor,
            host_output: *host_output,
            automation: automation.as_mut(),
            audit_replans,
            finalization_replans,
            audit_deferrals,
            lease: None,
            agents: std::collections::BTreeMap::new(),
            project_protection: None,
            drain_blocked: false,
            inspection_deferred: std::collections::BTreeMap::new(),
            host_failures: std::collections::BTreeMap::new(),
            unavailable_profiles: std::collections::BTreeMap::new(),
            tripped_hosts: std::collections::BTreeSet::new(),
            answered_hosts: std::collections::BTreeSet::new(),
            hosts_invoked: crate::commands::work::host::InvokedHosts::default(),
            host_probes: HostProbes::default(),
            recovery_turns: std::collections::BTreeSet::new(),
            audit_deferred_paths: std::collections::BTreeSet::new(),
            audit_batch: None,
            audit_wave_replans: std::collections::BTreeSet::new(),
            queue_turn: None,
            audit_turn: None,
            reserved_targets: Vec::new(),
            queue_replan: QueueReplan::None,
            writer_access_failed: false,
            started_at: Instant::now(),
            result: JobResult::new(operation),
        };
        if let Some(pin) = campaign_root.as_ref()
            && let Err(error) = pin.validate_target(&runner.root)
        {
            runner.failure("campaign vault", error);
            return runner.finish();
        }
        if !runner.configuration_is_current() {
            return runner.finish();
        }
        if matches!(operation, Operation::Daily) {
            let publication_workflow = if wait { "wait" } else { "refuse" };
            if board {
                runner
                    .native(
                        "task rollover",
                        &["task", "rollover", "--closing", "yesterday"],
                    )
                    .await;
            }
            if runner.config.ingest.recovery.is_some() {
                runner
                    .native(
                        "source recovery",
                        &[
                            "ingest",
                            "--recover",
                            "--wait-for-vault",
                            "--publication-workflow",
                            publication_workflow,
                        ],
                    )
                    .await;
            }
            runner
                .native(
                    "ingest",
                    &[
                        "ingest",
                        "--wait-for-vault",
                        "--publication-workflow",
                        publication_workflow,
                    ],
                )
                .await;
        }
        if !runner.result.cancelled && runner.acquire(wait).await {
            let pin = runner.lease.as_ref().expect("acquired workflow").root_pin();
            if let Some(expected) = campaign_root.as_ref() {
                if let Err(error) = expected.validate_target(pin.root()) {
                    runner.failure("campaign vault", error);
                    return runner.finish();
                }
            } else {
                *campaign_root = Some(pin.clone());
            }
            if runner.automation.is_some() && runner.config.invokes_semantic_hosts() {
                match execution::preflight_hosts(&runner.config.execution, operation.semantic()) {
                    Ok(unavailable) => runner.exclude_unavailable_hosts(unavailable),
                    Err(error) => {
                        runner.failure("host configuration", error);
                        return runner.finish();
                    }
                }
            }
            match operation {
                Operation::Daily => {
                    runner.drain().await;
                    if board {
                        runner.native("task propose", &["task", "propose"]).await;
                    }
                    runner.reconcile().await;
                    if runner.check_reviews() {
                        runner.resume_periods().await;
                        runner.reconcile().await;
                        runner.check_reviews();
                    }
                }
                Operation::Process => {
                    runner.reconcile().await;
                    runner.check_reviews();
                }
                Operation::Knowledge => runner.knowledge().await,
            }
        }
        runner.inspect_halted_state().await;
        runner.configuration_is_current();
        eprintln!(
            "Vault changes and Git staging remain available for explicit review; no automatic commit."
        );
        runner.finish()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn workflow_yield_requires_new_progress_and_keeps_failure_and_completion_distinct() {
            let mut result = JobResult::new(Operation::Process);
            result.ok = false;
            result.yield_reason = Some(YieldReason::WorkflowContention);
            assert_eq!(result.campaign_stop(), CampaignStop::NoProgress);
            result.work_telemetry.retained_progress.evidence_nodes = 1;
            assert_eq!(result.campaign_stop(), CampaignStop::Yielded);
            assert!(result.is_successful_yield());
            result.stages.push(StageResult {
                name: "required evidence".into(),
                state: StageState::Failed,
                detail: None,
                elapsed_job_ms: 0,
            });
            assert!(!result.is_successful_yield());
            result.halt_on("required evidence", HaltKind::Fault, None);
            assert_eq!(result.campaign_stop(), CampaignStop::Failed);
            result.cancelled = true;
            assert_eq!(result.campaign_stop(), CampaignStop::Cancelled);
        }

        #[test]
        fn input_drift_ends_the_batch_without_a_pause_and_a_fault_still_pauses() {
            for operation in [Operation::Process, Operation::Knowledge] {
                let mut result = JobResult::new(operation);
                result.ok = false;
                result.halt_on(
                    "audit fresh scope",
                    HaltKind::InputsChanged,
                    Some("removed"),
                );
                assert_eq!(
                    result
                        .unproductive_drift()
                        .map(|cause| cause.stage.as_str()),
                    Some("audit fresh scope"),
                    "drift without progress is what repeated-drift escalation counts"
                );
                result.work_telemetry.retained_progress.action_receipts = 1;
                assert!(result.unproductive_drift().is_none());
                assert_eq!(result.campaign_stop(), CampaignStop::InputsChanged);
                assert!(result.pause_cause().is_none());
                let report = serde_json::to_value(&result).unwrap();
                assert_eq!(report["halted"], true);
                assert_eq!(report["halt_kind"], "inputs-changed");
                result.halt_on("audit fresh scope", HaltKind::InputsChanged, Some("again"));
                result.halt_on(
                    "knowledge review health",
                    HaltKind::Fault,
                    Some("unreadable"),
                );
                assert_eq!(result.campaign_stop(), CampaignStop::Failed);
                assert_eq!(
                    result.pause_cause().map(|cause| cause.stage.as_str()),
                    Some("knowledge review health")
                );
                result.halt_on("audit fresh scope", HaltKind::InputsChanged, None);
                assert_eq!(serde_json::to_value(&result).unwrap()["halt_kind"], "fault");
            }
            let settled = serde_json::to_value(JobResult::new(Operation::Process)).unwrap();
            assert_eq!(settled["halted"], false);
            assert!(settled.get("halt_kind").is_none());
        }

        #[test]
        fn only_typed_input_drift_is_drift() {
            use crate::commands::queue::application::{RejectionKind, rejection};
            use lk_graph::GraphError;
            let path = || "wiki/concepts/a.md".to_owned();
            for drift in [
                miette::Report::new(crate::commands::work::SourceChanged::new("edited")),
                rejection(RejectionKind::SemanticContextChanged, "changed"),
                miette::Report::new(crate::commands::queue::TaskNotCurrent {
                    status: Some(crate::commands::queue::TaskStatus::Stale),
                    detail: path(),
                }),
                typed_error(GraphError::AuditInputChanged(path())),
                typed_error(GraphError::AuditScreeningChanged),
                typed_error(GraphError::AuditSubjectRemoved(path())),
                typed_error(GraphError::AuditSubjectInputChanged {
                    subject: path(),
                    input: path(),
                }),
                typed_error(lk_vault::VaultError::RssOriginChanged(path())),
                typed_error(lk_vault::VaultError::MutationConflict(path().into())),
                typed_error(GraphError::from(lk_vault::VaultError::MutationConflict(
                    path().into(),
                ))),
            ] {
                assert!(is_input_drift(&drift), "{drift}");
            }
            for fault in [
                rejection(RejectionKind::InvalidSection, "invalid"),
                miette::Report::new(crate::commands::queue::TaskNotCurrent {
                    status: Some(crate::commands::queue::TaskStatus::Unreadable),
                    detail: path(),
                }),
                miette::Report::new(crate::commands::queue::TaskNotCurrent {
                    status: Some(crate::commands::queue::TaskStatus::InvalidTarget),
                    detail: path(),
                }),
                typed_error(GraphError::Io("unreadable".into())),
                typed_error(lk_vault::VaultError::Original("missing payload".into())),
                miette::miette!("{}", GraphError::AuditSubjectRemoved(path())),
            ] {
                assert!(!is_input_drift(&fault), "{fault}");
            }
        }

        #[test]
        fn knowledge_continues_after_a_new_semantic_hold_but_stops_on_cached_holds() {
            let mut result = JobResult::new(Operation::Knowledge);
            result.ok = false;
            result.audit = Some(Vec::new());
            assert_eq!(result.campaign_stop(), CampaignStop::Deferred);
            result.work_telemetry.retained_progress.action_receipts = 1;
            assert_eq!(result.campaign_stop(), CampaignStop::BatchLimit);
            result.work_telemetry.retained_progress = RetainedProgress::default();
            assert_eq!(result.campaign_stop(), CampaignStop::Deferred);
        }

        #[test]
        fn process_campaign_requires_a_verified_budget_checkpoint() {
            let mut result = JobResult::new(Operation::Process);
            result.ok = false;
            result.budget_exhausted = true;
            assert_eq!(result.campaign_stop(), CampaignStop::NoProgress);
            result.work_telemetry.retained_progress.action_receipts = 1;
            assert_eq!(result.campaign_stop(), CampaignStop::BatchLimit);
            result.halt_on("queue application", HaltKind::Fault, None);
            assert_eq!(result.campaign_stop(), CampaignStop::Failed);
            result.cancelled = true;
            assert_eq!(result.campaign_stop(), CampaignStop::Cancelled);
            result.cancelled = false;
            result.halt = HaltState::default();
            result.budget_exhausted = false;
            assert_eq!(result.campaign_stop(), CampaignStop::Deferred);
            result.ok = true;
            assert_eq!(result.campaign_stop(), CampaignStop::Settled);
        }

        #[test]
        fn a_host_fault_ends_the_campaign_even_after_progress_or_a_yield() {
            for operation in [Operation::Process, Operation::Knowledge] {
                let mut result = JobResult::new(operation);
                result.ok = false;
                result.budget_exhausted = true;
                result.work_telemetry.retained_progress.action_receipts = 1;
                result.yield_reason = Some(YieldReason::WorkflowContention);
                result.host_fault = true;
                assert_eq!(result.campaign_stop(), CampaignStop::HostFault);
            }
        }

        #[test]
        fn retained_progress_survives_truncated_telemetry_without_counting_cached_work() {
            let mut telemetry = WorkTelemetry::default();
            assert!(!telemetry.retained_progress.has_progress());
            for _ in 0..WorkTelemetry::MAX_STEPS + 2 {
                let mut step = WorkStepMetrics::new(WorkRole::Evidence);
                step.outcome = WorkOutcome::Recorded;
                telemetry.record(step);
            }
            for outcome in [
                WorkOutcome::AlreadyComplete,
                WorkOutcome::Restaged,
                WorkOutcome::Rejected,
                WorkOutcome::Failed,
                WorkOutcome::Cancelled,
            ] {
                let mut step = WorkStepMetrics::new(WorkRole::Evidence);
                step.outcome = outcome;
                telemetry.record(step);
            }
            assert_eq!(telemetry.work_steps.len(), WorkTelemetry::MAX_STEPS);
            assert_eq!(telemetry.work_steps_omitted, 7);
            assert_eq!(
                telemetry.retained_progress.evidence_nodes,
                WorkTelemetry::MAX_STEPS + 2
            );
            assert_eq!(telemetry.retained_progress.screening_packets, 0);
            assert_eq!(telemetry.retained_progress.action_receipts, 0);
            assert!(telemetry.retained_progress.has_progress());
        }

        #[test]
        fn finalization_admission_diagnostics_do_not_count_as_calls_or_progress() {
            use crate::commands::work::native::{
                FinalizationAdmission, FinalizationAdmissionState, FinalizationMode,
            };
            let resource = lk_core::semantic::SemanticResource {
                address: "wiki/documents/source.md".into(),
                revision: "source-revision".into(),
                authority: lk_core::semantic::SemanticAuthority::Page,
                text: "Declared source claim.".into(),
            };
            let work = crate::commands::work::state::Work::build(
                &[resource],
                "captured-subject",
                serde_json::json!({
                    "subject":{"kind":"queue","task_id":"task","cache_hash":"hash","target":"wiki/documents/test.md","target_revision":"target"},
                    "purpose":{"task_kind":"extract-concepts"}
                }),
                crate::commands::work::state::WorkLimits {
                    max_input_bytes: 3000,
                    max_result_bytes: 1024,
                },
            )
            .unwrap();
            let task: lk_queue::QueueTask = serde_json::from_value(serde_json::json!({
                "task_id":"task", "kind":"extract-concepts", "created_at":"2026-10-24T00:00:00Z",
                "cache_hash":"hash", "input":{"text":"source","locale":"ko","source_type":"manual","concept_policy":"supported-new","date":"2026-10-24"},
                "target":{"vault_path":"wiki/documents/test.md","anchor":"## Related Concepts","kind":"document-concepts"}
            })).unwrap();
            let mut result = JobResult::new(Operation::Process);
            for _ in 0..WorkTelemetry::MAX_STEPS + 2 {
                result
                    .work_telemetry
                    .record_admission(FinalizationAdmission::new(
                        &work,
                        &task,
                        FinalizationMode::Frontier,
                        FinalizationAdmissionState::Available {
                            request_bytes: Some(1234),
                        },
                    ));
            }
            assert_eq!(result.semantic_calls, 0);
            assert!(!result.work_telemetry.retained_progress.has_progress());
            assert!(result.work_telemetry.work_steps.is_empty());
            let json = serde_json::to_value(result).unwrap();
            assert_eq!(
                json["finalization_admissions"].as_array().unwrap().len(),
                WorkTelemetry::MAX_STEPS
            );
            assert_eq!(json["finalization_admissions_omitted"], 2);
            assert_eq!(
                json["finalization_admissions"][0]["work_subject_revision"],
                "captured-subject"
            );
            assert_eq!(json["finalization_admissions"][0]["request_bytes"], 1234);
        }

        #[test]
        fn work_telemetry_is_bounded_and_preserves_unobserved_nulls() {
            let mut telemetry = WorkTelemetry::default();
            for index in 0..WorkTelemetry::MAX_STEPS + 2 {
                let mut step = WorkStepMetrics::new(WorkRole::Evidence);
                step.work_id = Some(index.to_string());
                telemetry.record(step);
            }
            let json = serde_json::to_value(telemetry).unwrap();
            let steps = json["work_steps"].as_array().unwrap();
            assert_eq!(steps.len(), WorkTelemetry::MAX_STEPS);
            assert_eq!(json["work_steps_omitted"], 2);
            assert_eq!(steps[0]["work_id"], "0");
            assert_eq!(steps.last().unwrap()["work_id"], "1023");
            for field in [
                "request_json_bytes",
                "result_json_bytes",
                "preparation_duration_ms",
                "agent_duration_ms",
                "revalidation_record_duration_ms",
                "verification_duration_ms",
            ] {
                assert!(steps[0][field].is_null(), "{field}");
            }
        }
    }
}
