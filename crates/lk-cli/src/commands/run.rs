use clap::ValueEnum;
use serde::Serialize;

#[cfg(unix)]
mod scheduler;

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
    operation: Operation,
    profile: Option<String>,
    batches: usize,
    wait: bool,
    json: bool,
) -> miette::Result<()> {
    #[cfg(unix)]
    {
        native::run(opts, installation, operation, profile, batches, wait, json).await
    }
    #[cfg(not(unix))]
    {
        let _ = (opts, installation, operation, profile, batches, wait, json);
        Err(miette::miette!(
            "agent jobs require Unix workflow leases; Windows is unsupported"
        ))
    }
}

#[cfg(unix)]
mod native {
    use super::scheduler;
    use super::*;
    use crate::commands::audit::{DeferredAudit, InputAvailability};
    use crate::commands::{
        CONFIG_REVISION_ENV, EXECUTION_PROFILE_ENV, GlobalOptions, execution, workflow,
    };
    use lk_core::config::Config;
    use lk_core::execution::ExecutionProfile;
    use miette::IntoDiagnostic;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum WorkRole {
        Evidence,
        Screening,
        Finalizer,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum NodeKind {
        Inspect,
        Reduce,
    }

    #[derive(Clone, Copy, Serialize)]
    #[serde(rename_all = "kebab-case")]
    enum WorkOutcome {
        Recorded,
        Staged,
        Restaged,
        AlreadyComplete,
        Rejected,
        Failed,
        Cancelled,
    }

    #[derive(Serialize)]
    struct WorkStepMetrics {
        role: WorkRole,
        work_id: Option<String>,
        node_id: Option<String>,
        node_kind: Option<NodeKind>,
        selection_id: Option<String>,
        batch_id: Option<String>,
        // Compact UTF-8 JSON, excluding prompts and host envelopes. Results measure
        // the decoded value before recording; interactive finalizers are unobserved.
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
                work_id: None,
                node_id: None,
                node_kind: None,
                selection_id: None,
                batch_id: None,
                request_json_bytes: None,
                result_json_bytes: None,
                preparation_duration_ms: None,
                preparation_from_queue_plan: false,
                agent_duration_ms: None,
                revalidation_record_duration_ms: None,
                verification_duration_ms: None,
                outcome: WorkOutcome::Rejected,
            }
        }
    }

    #[derive(Default, Serialize)]
    struct RetainedProgress {
        evidence_nodes: usize,
        screening_packets: usize,
        staged_requests: usize,
        discovery_recipes: usize,
        action_receipts: usize,
        queue_publication_observed: bool,
    }

    impl RetainedProgress {
        fn has_progress(&self) -> bool {
            self.evidence_nodes != 0
                || self.screening_packets != 0
                || self.staged_requests != 0
                || self.discovery_recipes != 0
                || self.action_receipts != 0
                || self.queue_publication_observed
        }
    }

    #[derive(Default, Serialize)]
    struct WorkTelemetry {
        retained_progress: RetainedProgress,
        work_steps: Vec<WorkStepMetrics>,
        work_steps_omitted: usize,
        evidence_preparation_io: lk_vault::DiscoveryCaptureMetrics,
        evidence_preparation_duration_ms: u64,
        evidence_revalidation_io: lk_vault::DiscoveryCaptureMetrics,
        evidence_revalidation_duration_ms: u64,
        evidence_record_duration_ms: u64,
        screening_preparation_io: lk_vault::DiscoveryCaptureMetrics,
        screening_revalidation_io: lk_vault::DiscoveryCaptureMetrics,
        screening_revalidation_duration_ms: u64,
        screening_record_duration_ms: u64,
    }

    impl WorkTelemetry {
        const MAX_STEPS: usize = 1024;

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

    struct ObservationReply {
        engine: lk_core::execution::AgentEngine,
        bytes: Vec<u8>,
        limit: usize,
    }

    enum ObservationStep {
        Recorded,
        Deferred(String),
        Complete {
            root_node: String,
            preparation_duration_ms: u64,
        },
        Failed,
    }

    struct PreparedObservations {
        completed: usize,
        requests: std::collections::VecDeque<serde_json::Value>,
    }

    struct ObservationTurn {
        work_id: String,
        node_id: String,
        completed: usize,
        request: serde_json::Value,
        metrics: WorkStepMetrics,
    }

    struct ScreeningAttempt {
        task_id: String,
        completed_batches: usize,
        batches: usize,
        request: serde_json::Value,
        contract_revision: String,
        metrics: WorkStepMetrics,
    }

    struct ScreeningReply {
        task_id: String,
        output: crate::commands::work::ScreeningResponse,
        completed_batches: usize,
        batches: usize,
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
            selection_id: String,
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
                Self::Screening { selection_id, .. } => format!("screening:{selection_id}"),
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

    fn is_interactive_batch_candidate(
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
    }

    #[derive(Serialize)]
    struct ExecutionInfo {
        profile: String,
        engine: lk_core::execution::AgentEngine,
        version: String,
        requested_model: Option<String>,
        work: lk_core::execution::SemanticWorkConfig,
    }

    #[derive(Default, Serialize)]
    struct ReviewHealth {
        open: usize,
        resolved: usize,
        needs_reassessment: usize,
        invalid: usize,
    }

    #[derive(Serialize)]
    struct AuditProgress {
        before: lk_graph::audit::AuditCoverage,
        after: Option<lk_graph::audit::AuditCoverage>,
    }

    #[derive(Serialize)]
    struct JobResult {
        operation: Operation,
        ok: bool,
        cancelled: bool,
        halted: bool,
        duration_ms: u64,
        semantic_duration_ms: u64,
        execution: Option<ExecutionInfo>,
        workflow_id: Option<lk_core::execution::WorkflowId>,
        semantic_calls: usize,
        budget_exhausted: bool,
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
        review_health: Option<ReviewHealth>,
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
        Cancelled,
    }

    impl JobResult {
        fn new(operation: Operation) -> Self {
            Self {
                operation,
                ok: true,
                cancelled: false,
                halted: false,
                duration_ms: 0,
                semantic_duration_ms: 0,
                execution: None,
                workflow_id: None,
                semantic_calls: 0,
                budget_exhausted: false,
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

        fn campaign_stop(&self) -> CampaignStop {
            if self.cancelled {
                return CampaignStop::Cancelled;
            }
            if matches!(self.operation, Operation::Process) {
                return if self.halted {
                    CampaignStop::Failed
                } else if self.ok {
                    CampaignStop::Settled
                } else if self.budget_exhausted {
                    if self.work_telemetry.retained_progress.has_progress() {
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
                if self.halted {
                    CampaignStop::Failed
                } else if self
                    .audit
                    .as_ref()
                    .is_some_and(|completed| !completed.is_empty())
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
    }

    struct Runner<'a> {
        opts: &'a GlobalOptions,
        installation: &'a lk_dist::Installation,
        config: Config,
        config_revision: String,
        configuration_failed: bool,
        root: PathBuf,
        supervisor: &'a mut execution::Supervisor,
        lease: Option<workflow::WorkflowLease>,
        selected: Option<ExecutionProfile>,
        project_protection: Option<crate::commands::installation::Protection>,
        drain_blocked: bool,
        inspection_deferred: std::collections::BTreeMap<String, String>,
        audit_batch: Option<Vec<lk_graph::audit::AuditCandidate>>,
        queue_turn: Option<QueueTurn>,
        queue_admitted: scheduler::AdmittedTurns,
        queue_replan: bool,
        writer_access_failed: bool,
        started_at: Instant,
        result: JobResult,
    }

    impl Runner<'_> {
        fn finish(mut self) -> JobResult {
            self.result.duration_ms = self.started_at.elapsed().as_millis() as u64;
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
            });
            success
        }

        fn failure(&mut self, name: &str, error: impl std::fmt::Display) -> bool {
            if self.supervisor.is_cancelled() {
                return self.record(name, StageState::Cancelled, Some(error.to_string()));
            }
            self.result.halted = true;
            self.record(name, StageState::Failed, Some(error.to_string()))
        }

        fn record_work_step(&mut self, mut step: WorkStepMetrics) {
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
            self.result.work_telemetry.record(step);
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
            self.failure("configuration", failure)
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
            if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            self.configure_child(&mut command);
            let completed = match self
                .supervisor
                .run_request(command, timeout, None, input)
                .await
            {
                Ok(result) => {
                    if !result.success && self.lease.is_some() {
                        self.result.halted = true;
                    }
                    self.record(
                        name,
                        if result.cancelled {
                            StageState::Cancelled
                        } else if result.success {
                            StageState::Completed
                        } else {
                            StageState::Failed
                        },
                        result.detail,
                    )
                }
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
            if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            if args == ["queue", "apply"] {
                return self.apply_queue(name).await;
            }
            if args == ["queue", "check"] {
                return self.check_queue(name);
            }
            let mut command_args = args.to_vec();
            if args == ["schema"]
                || args == ["graph", "backlinks-sync"]
                || args == ["wiki", "refresh"]
                || args.starts_with(&["wiki", "audit-record"])
            {
                command_args.push("--wait-for-vault");
            }
            match execution::lore_command(self.opts, &command_args) {
                Ok(command) => self.child(name, command, None).await,
                Err(error) => self.failure(name, error),
            }
        }

        fn check_queue(&mut self, name: &str) -> bool {
            let state = match crate::commands::queue::check::inspect(&self.root) {
                Ok(state) => state,
                Err(error) => return self.failure(name, error),
            };
            if state.pending_mutation || state.unreadable != 0 || state.unparseable_lines != 0 {
                return self.failure(name, "queue contains an interrupted mutation or unknown inputs; inspect `lore queue check`");
            }
            if !state.staged_results.is_empty()
                && let Err(error) =
                    crate::commands::queue::readiness::InputReadiness::load(&self.root)
            {
                return self.failure(name, error);
            }
            if state.is_settled() {
                return self.record(name, StageState::Completed, None);
            }
            self.deferred(
                name,
                format!(
                    "{} current tasks and {} staged results remain; inspect `lore queue check`",
                    state.current,
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
                .run_with_output(command, None, Some(8 * 1024 * 1024))
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
                            format!("queue application report is unavailable or invalid: {error}"),
                        );
                    }
                };
            if outcome.success != report.ok || outcome.exit_code != Some(i32::from(!report.ok)) {
                return self.failure(
                    name,
                    "queue application report disagrees with its process exit",
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
            let detail = format!(
                "{} applied, {} dropped, {} rejected; {}",
                report.applied,
                report.dropped,
                report.rejected,
                if can_continue {
                    "independent work may continue"
                } else {
                    "unsafe or unknown state stops further work"
                }
            );
            let success = report.ok;
            self.result.queue_application.latest = Some(report);
            self.record(
                name,
                if success {
                    StageState::Completed
                } else {
                    StageState::Failed
                },
                Some(detail),
            );
            self.result.halted |= !can_continue;
            can_continue
        }

        async fn acquire(&mut self, wait: bool) -> bool {
            if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            match self
                .supervisor
                .acquire(
                    self.opts,
                    &self.config,
                    &self.root,
                    &self.config_revision,
                    wait,
                )
                .await
            {
                Ok(Some(lease)) => {
                    self.root = lease.root_pin().root().to_path_buf();
                    self.config.vault.root = self.root.clone();
                    self.result.workflow_id = Some(lease.id().clone());
                    self.lease = Some(lease);
                    self.configuration_is_current()
                }
                Ok(None) => self.record(
                    "workflow lease",
                    StageState::Cancelled,
                    Some("cancelled while waiting".into()),
                ),
                Err(error) => self.failure("workflow lease", error),
            }
        }

        async fn mutation_session(
            &mut self,
            name: &str,
        ) -> Option<(lk_vault::MutationSession, bool)> {
            if self.result.halted {
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
                .acquire_mutation(&self.root, || {
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

        async fn prepare_agent(&mut self) -> miette::Result<()> {
            if self.project_protection.is_none() {
                self.project_protection =
                    Some(crate::commands::installation::Protection::native_project(
                        self.installation,
                        self.opts,
                        &self.root,
                    )?);
            }
            if self.selected.is_some() {
                return Ok(());
            }
            let (name, profile) = self
                .config
                .execution
                .resolve_profile(
                    self.result.operation.semantic(),
                    self.config.execution.selected_profile(),
                )
                .map_err(|e| miette::miette!("{e}"))?;
            let name = name.to_owned();
            let mut profile = profile.clone();
            let executable = execution::executable_path(profile.executable())?;
            profile.set_executable(executable.clone());
            let mut command = tokio::process::Command::new(executable);
            command.arg("--version");
            let version = self
                .supervisor
                .run(
                    command,
                    Some(Duration::from_secs(10).min(profile.timeout())),
                    true,
                )
                .await?;
            if version.cancelled {
                self.result.cancelled = true;
            }
            if !version.success {
                return Err(miette::miette!(
                    "agent preflight failed: {}",
                    version
                        .detail
                        .unwrap_or_else(|| "version unavailable".into())
                ));
            }
            execution::verify_version(profile.engine(), &version.stdout)?;
            self.result.execution = Some(ExecutionInfo {
                profile: name,
                engine: profile.engine(),
                version: String::from_utf8(version.stdout)
                    .map_err(|e| miette::miette!("{e}"))?
                    .trim()
                    .into(),
                requested_model: profile.model().map(str::to_owned),
                work: self
                    .config
                    .execution
                    .effective_work()
                    .map_err(|error| miette::miette!("{error}"))?,
            });
            self.selected = Some(profile);
            Ok(())
        }

        async fn consumer(
            &mut self,
            skill: &str,
            operation: &str,
            input_directory: Option<&Path>,
        ) -> bool {
            if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                return self.failure(skill, "semantic execution is disabled by llm.provider");
            }
            if !self.call_budget_available() {
                return false;
            }
            if let Err(error) = self.prepare_agent().await {
                return if self.result.cancelled {
                    self.record(skill, StageState::Cancelled, Some(error.to_string()))
                } else {
                    self.failure(skill, error)
                };
            }
            let profile = self.selected.as_ref().expect("prepared agent");
            let timeout = profile.timeout();
            let invocation = execution::agent_invocation(
                profile,
                skill,
                operation,
                self.installation,
                self.opts.config.as_ref().expect("resolved config"),
                input_directory,
                None,
            );
            match invocation {
                Ok(execution::AgentInvocation { mut command, input }) => {
                    if !self.reserve_queue_turn().await {
                        return false;
                    }
                    self.result.semantic_calls += 1;
                    command.current_dir(&self.root);
                    let started_at = Instant::now();
                    let completed = self
                        .child_with_input(skill, command, Some(timeout), input.as_deref())
                        .await;
                    self.result.semantic_duration_ms += started_at.elapsed().as_millis() as u64;
                    completed
                }
                Err(error) => self.failure(skill, error),
            }
        }

        fn observation_invocation(
            &self,
            request: &serde_json::Value,
            role: WorkRole,
            budget: lk_core::execution::SemanticWorkConfig,
        ) -> miette::Result<(execution::observation::Invocation, usize)> {
            let profile = self.selected.as_ref().expect("prepared agent");
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
            }?;
            invocation
                .command
                .env("LORE_INSTALL_DATA_DIR", self.installation.data_dir());
            if let Some(lease) = &self.lease {
                lease.configure(invocation.command.as_std_mut());
            }
            invocation
                .command
                .env(CONFIG_REVISION_ENV, &self.config_revision)
                .env(
                    "LORE_CONFIG",
                    self.opts.config.as_ref().expect("resolved config"),
                );
            if let Some(name) = self.config.execution.selected_profile() {
                invocation.command.env(EXECUTION_PROFILE_ENV, name);
            }
            let limit = budget
                .max_result_bytes
                .checked_mul(3)
                .and_then(|limit| limit.checked_add(16384))
                .ok_or_else(|| miette::miette!("host output envelope limit overflow"))?;
            Ok((invocation, limit))
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
            if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return None;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                self.failure(
                    "bounded work receipt",
                    "semantic execution is disabled by llm.provider",
                );
                return None;
            }
            let budget = match self.config.execution.effective_work() {
                Ok(budget) => budget,
                Err(error) => {
                    self.failure("semantic work budget", error);
                    return None;
                }
            };
            if !self.call_budget_available() {
                return None;
            }
            if let Err(error) = self.prepare_agent().await {
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
            let (invocation, limit) =
                match self.observation_invocation(&request, metrics.role, budget) {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        self.failure("bounded work receipt", error);
                        return None;
                    }
                };
            metrics.request_json_bytes = Some(invocation.request_json_bytes);
            let timeout = self.selected.as_ref().expect("prepared agent").timeout();
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
                        request["selection_id"]
                            .as_str()
                            .expect("prepared selection identity")
                    ),
                    WorkRole::Finalizer => format!(
                        "work:{}",
                        request["work"]["work_id"]
                            .as_str()
                            .expect("prepared finalizer identity")
                    ),
                };
                if binding.identity != actual {
                    self.queue_admitted.clear();
                    self.queue_replan = true;
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
            if !self.reserve_queue_turn().await {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return None;
            }
            self.result.semantic_calls += 1;
            let started_at = Instant::now();
            let outcome = self
                .supervisor
                .run_request(
                    invocation.command,
                    Some(timeout),
                    Some(limit),
                    Some(&invocation.prompt),
                )
                .await;
            let duration_ms = started_at.elapsed().as_millis() as u64;
            metrics.agent_duration_ms = Some(duration_ms);
            self.result.semantic_duration_ms += duration_ms;
            let child = match outcome {
                Ok(child) => child,
                Err(error) => {
                    metrics.outcome = WorkOutcome::Failed;
                    self.failure("bounded work receipt", error);
                    return None;
                }
            };
            if !child.success {
                self.result.halted |= !child.cancelled;
                metrics.outcome = if child.cancelled {
                    WorkOutcome::Cancelled
                } else {
                    WorkOutcome::Failed
                };
                self.record(
                    "bounded work receipt",
                    if child.cancelled {
                        StageState::Cancelled
                    } else {
                        StageState::Failed
                    },
                    child.detail,
                );
                self.configuration_is_current();
                return None;
            }
            if !self.configuration_is_current() {
                return None;
            }
            Some(ObservationReply {
                engine: invocation.engine,
                bytes: child.stdout,
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
            self.record_observation(id, node, &request, completed, reply, metrics)
                .await
        }

        fn decode_observation_reply(
            &mut self,
            request: &serde_json::Value,
            reply: ObservationReply,
            metrics: &mut WorkStepMetrics,
        ) -> Option<crate::commands::work::InspectionResponse> {
            let output = match execution::observation::decode_response(
                reply.engine,
                &reply.bytes,
                reply.limit,
            ) {
                Ok(output) => output,
                Err(error) => {
                    if let Some(capacity) =
                        error.downcast_ref::<crate::commands::work::state::OutputCapacity>()
                    {
                        metrics.result_json_bytes = Some(capacity.actual_bytes);
                        return Some(crate::commands::work::InspectionResponse::Capacity(
                            capacity.clone(),
                        ));
                    }
                    self.failure("bounded work receipt", error);
                    return None;
                }
            };
            metrics.result_json_bytes = Some(
                serde_json::to_vec(&output)
                    .expect("decoded observation serializes")
                    .len(),
            );
            match execution::observation::decode_observation(output, request, reply.limit) {
                Ok(execution::observation::ObservationResponse::Complete(output)) => {
                    Some(crate::commands::work::InspectionResponse::Complete(output))
                }
                Ok(execution::observation::ObservationResponse::NeedsAction(reason)) => Some(
                    crate::commands::work::InspectionResponse::NeedsAction(reason),
                ),
                Err(error) => {
                    if let Some(capacity) =
                        error.downcast_ref::<crate::commands::work::state::OutputCapacity>()
                    {
                        return Some(crate::commands::work::InspectionResponse::Capacity(
                            capacity.clone(),
                        ));
                    }
                    self.failure("bounded work receipt", error);
                    None
                }
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
            match crate::commands::work::record_inspections(
                &session,
                &self.config,
                &self.config_revision,
                replies,
                self.supervisor.read_control(),
            ) {
                Ok(recorded) => {
                    let record_ms = recorded
                        .records
                        .iter()
                        .map(|record| record.duration_ms)
                        .sum::<u64>();
                    self.result
                        .work_telemetry
                        .evidence_revalidation_io
                        .accumulate(recorded.discovery_io);
                    self.result.work_telemetry.evidence_revalidation_duration_ms +=
                        recorded.revalidation_duration_ms;
                    self.result.work_telemetry.evidence_record_duration_ms += record_ms;
                    let shared_ms =
                        (started.elapsed().as_millis() as u64).saturating_sub(record_ms);
                    Some((Ok(recorded), shared_ms))
                }
                Err(error) => Some((Err(error), started.elapsed().as_millis() as u64)),
            }
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
                Ok(InspectionRecordOutcome::Recorded(progress)) => {
                    if progress.completed <= completed
                        || progress.ready.iter().any(|ready| ready == node)
                    {
                        self.failure("bounded work receipt", "worker exited without completing its selected node; unfinished work retained");
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
                            ("bounded work action", reason)
                        }
                        InspectionRecordOutcome::Rejected(reason) => {
                            metrics.outcome = WorkOutcome::Rejected;
                            ("bounded output capacity", reason)
                        }
                        InspectionRecordOutcome::Recorded(_) => unreachable!(),
                    };
                    self.result.work_telemetry.retained_progress.action_receipts += 1;
                    self.inspection_deferred.insert(id.into(), reason.clone());
                    self.deferred(stage, &reason);
                    ObservationStep::Deferred(reason)
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

        fn call_budget_available(&mut self) -> bool {
            match self.config.execution.effective_work() {
                Ok(budget) if self.result.semantic_calls < budget.max_calls => true,
                Ok(_) => {
                    self.result.budget_exhausted = true;
                    self.deferred("semantic call budget", "configured execution.work.max_calls exhausted; unfinished work is retained for the next run")
                }
                Err(error) => self.failure("semantic work budget", error),
            }
        }

        async fn reserve_queue_turn(&mut self) -> bool {
            let Some(turn) = self.queue_turn.take() else {
                return true;
            };
            let Some((session, waited)) = self.mutation_session("queue scheduling").await else {
                return false;
            };
            if waited {
                self.queue_admitted.clear();
                self.queue_replan = true;
                self.record("queue input refresh", StageState::Skipped, Some("writer access became available; preparing current inputs before invocation".into()));
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
                Err(error) => return Some(self.failure("semantic work budget", error)),
            };
            let limit = budget
                .max_parallel
                .min(budget.max_calls.saturating_sub(self.result.semantic_calls));
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
            let captured = match crate::commands::work::prepare_inspections_batch(
                &self.root,
                &self.config,
                &self.config_revision,
                &demands,
                self.supervisor.read_control(),
            ) {
                Ok(captured) => captured,
                Err(error) => return Some(self.failure("bounded work progress", error)),
            };
            self.result
                .work_telemetry
                .evidence_preparation_io
                .accumulate(captured.discovery_io);
            self.result.work_telemetry.evidence_preparation_duration_ms += captured.duration_ms;
            let mut preparation_duration_ms = Some(captured.duration_ms);
            for ((id, _), result) in demands.into_iter().zip(captured.turns) {
                let (progress, requests) = match result {
                    Ok(captured) => captured,
                    Err(error)
                        if error
                            .downcast_ref::<crate::commands::work::state::InputCapacity>()
                            .is_some() =>
                    {
                        return None;
                    }
                    Err(error) => return Some(self.failure("bounded work progress", error)),
                };
                if !progress.complete && requests.is_empty() {
                    return Some(
                        self.failure("bounded work progress", "incomplete work has no ready node"),
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
            let mut turns = Vec::new();
            let mut reserved_targets = Vec::new();
            for target in planned_targets {
                let Some(id) = jobs.get(&target) else { break };
                let work = prepared.get_mut(id).expect("captured work");
                let Some(request) = work.requests.pop_front() else {
                    break;
                };
                match crate::commands::work::inspection_action(&self.root, id, &request) {
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
                    work_id: id.clone(),
                    node_id,
                    completed: work.completed,
                    request,
                    metrics,
                });
            }
            if turns.is_empty() {
                return if self.inspection_deferred.contains_key(&jobs[selected]) {
                    Some(true)
                } else {
                    None
                };
            }
            if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                return Some(false);
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                return Some(self.failure(
                    "bounded work receipt",
                    "semantic execution is disabled by llm.provider",
                ));
            }
            if let Err(error) = self.prepare_agent().await {
                for mut turn in turns {
                    turn.metrics.outcome = if self.result.cancelled {
                        WorkOutcome::Cancelled
                    } else {
                        WorkOutcome::Failed
                    };
                    self.record_work_step(turn.metrics);
                }
                return Some(if self.result.cancelled {
                    self.record(
                        "bounded work receipt",
                        StageState::Cancelled,
                        Some(error.to_string()),
                    )
                } else {
                    self.failure("bounded work receipt", error)
                });
            }
            let timeout = self.selected.as_ref().expect("prepared agent").timeout();
            let mut requests = Vec::new();
            let mut directories = Vec::new();
            let mut engines = Vec::new();
            for turn in &mut turns {
                let (invocation, capture_limit) =
                    match self.observation_invocation(&turn.request, WorkRole::Evidence, budget) {
                        Ok(invocation) => invocation,
                        Err(error) => return Some(self.failure("bounded work receipt", error)),
                    };
                turn.metrics.request_json_bytes = Some(invocation.request_json_bytes);
                engines.push(invocation.engine);
                let (request, directory) = invocation.into_request(timeout, capture_limit);
                requests.push(request);
                directories.push(directory);
            }
            let reservations = &reserved_targets[1..];
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
                            return Some(self.failure("bounded work scheduling", error));
                        }
                    }
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
            self.result.semantic_calls += requests.len();
            let attempts = self.supervisor.run_requests(requests).await;
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

        async fn settle_observation_attempts(
            &mut self,
            turns: Vec<ObservationTurn>,
            attempts: Vec<execution::ChildAttempt>,
            engines: Vec<lk_core::execution::AgentEngine>,
            result_limit: usize,
            cancelled: bool,
        ) -> bool {
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
            for ((mut turn, attempt), engine) in turns.into_iter().zip(attempts).zip(engines) {
                turn.metrics.agent_duration_ms = Some(attempt.duration_ms);
                self.result.semantic_duration_ms += attempt.duration_ms;
                if cancelled || !current {
                    turn.metrics.outcome = if cancelled {
                        WorkOutcome::Cancelled
                    } else {
                        WorkOutcome::Rejected
                    };
                    self.record_work_step(turn.metrics);
                    continue;
                }
                let child = match attempt.result {
                    Ok(child) if child.success => child,
                    Ok(child) => {
                        accepted = false;
                        self.result.halted = true;
                        turn.metrics.outcome = WorkOutcome::Failed;
                        self.record("bounded work receipt", StageState::Failed, child.detail);
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
                };
                if !self.configuration_is_current() {
                    accepted = false;
                    self.record_work_step(turn.metrics);
                    continue;
                }
                let Some(response) = self.decode_observation_reply(
                    &turn.request,
                    ObservationReply {
                        engine,
                        bytes: child.stdout,
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
            if !replies.is_empty() {
                match self.record_observation_batch(replies).await {
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
                            if matches!(step, ObservationStep::Failed) {
                                accepted = false;
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
            accepted
        }

        fn record_uninvoked_observations(&mut self, turns: Vec<ObservationTurn>) {
            for mut turn in turns {
                if self.result.cancelled {
                    turn.metrics.outcome = WorkOutcome::Cancelled;
                }
                self.record_work_step(turn.metrics);
            }
        }

        async fn bounded_observation_step(&mut self, id: &str) -> ObservationStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Evidence);
            metrics.work_id = Some(id.into());
            let started_at = Instant::now();
            let prepared = crate::commands::work::prepare_inspection(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
                self.supervisor.read_control(),
            );
            let preparation_duration_ms = started_at.elapsed().as_millis() as u64;
            metrics.preparation_duration_ms = Some(preparation_duration_ms);
            let (before, request) = match prepared {
                Ok(status) => status,
                Err(error)
                    if error
                        .downcast_ref::<crate::commands::work::state::InputCapacity>()
                        .is_some() =>
                {
                    let reason = error.to_string();
                    self.deferred("bounded work capacity", &reason);
                    self.record_work_step(metrics);
                    return ObservationStep::Deferred(reason);
                }
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("bounded work progress", error);
                    return ObservationStep::Failed;
                }
            };
            if !before.complete {
                let Some(node) = before.ready.first() else {
                    self.record_work_step(metrics);
                    self.failure("bounded work progress", "incomplete work has no ready node");
                    return ObservationStep::Failed;
                };
                metrics.node_id = Some(node.clone());
                let Some(request) = request else {
                    self.record_work_step(metrics);
                    self.failure(
                        "bounded work progress",
                        "ready work has no inspection input",
                    );
                    return ObservationStep::Failed;
                };
                metrics.node_kind = match request["kind"].as_str() {
                    Some("inspect") => Some(NodeKind::Inspect),
                    Some("reduce") => Some(NodeKind::Reduce),
                    _ => None,
                };
                let result =
                    match crate::commands::work::inspection_action(&self.root, id, &request) {
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
                return result;
            }
            ObservationStep::Complete {
                root_node: before.root_node,
                preparation_duration_ms,
            }
        }

        async fn request_result(
            &mut self,
            request: &serde_json::Value,
            metrics: &mut WorkStepMetrics,
        ) -> Option<serde_json::Value> {
            metrics.request_json_bytes = Some(
                serde_json::to_vec(request)
                    .expect("request serializes")
                    .len(),
            );
            if self.result.halted
                || self.result.cancelled
                || !self.configuration_is_current()
                || !self.call_budget_available()
            {
                return None;
            }
            let budget = match self.config.execution.effective_work() {
                Ok(budget) => budget,
                Err(error) => {
                    self.failure("native result budget", error);
                    return None;
                }
            };
            if metrics.request_json_bytes.expect("measured request") > budget.max_input_bytes {
                self.failure(
                    "native result request",
                    "complete request exceeds max_input_bytes",
                );
                return None;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                self.failure(
                    "native result",
                    "semantic execution is disabled by llm.provider",
                );
                return None;
            }
            if let Err(error) = self.prepare_agent().await {
                self.failure("native result host", error);
                return None;
            }
            let profile = self.selected.as_ref().expect("prepared host");
            let timeout = profile.timeout();
            let invocation = execution::result::build(
                profile,
                self.installation,
                self.opts.config.as_ref().expect("resolved config"),
                request,
                &crate::commands::work::generation::schema(),
                budget.max_result_bytes,
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
            if !self.reserve_queue_turn().await {
                return None;
            }
            self.result.semantic_calls += 1;
            let started = Instant::now();
            let child = self
                .supervisor
                .run_request(
                    agent.command,
                    Some(timeout),
                    output.capture_limit,
                    agent.input.as_deref(),
                )
                .await;
            let duration = started.elapsed().as_millis() as u64;
            metrics.agent_duration_ms = Some(duration);
            self.result.semantic_duration_ms += duration;
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    metrics.outcome = WorkOutcome::Failed;
                    self.failure("native result host", error);
                    return None;
                }
            };
            if child.cancelled {
                metrics.outcome = WorkOutcome::Cancelled;
                self.record("native result host", StageState::Cancelled, child.detail);
                return None;
            }
            if !self.configuration_is_current() {
                return None;
            }
            if !child.success {
                metrics.outcome = WorkOutcome::Failed;
                self.failure(
                    "native result host",
                    child.detail.unwrap_or_else(|| "host failed".into()),
                );
                return None;
            }
            match output.response(
                &child.stdout,
                request["max_response_bytes"]
                    .as_u64()
                    .and_then(|limit| usize::try_from(limit).ok())
                    .expect("native response allowance"),
            ) {
                Ok(value) => {
                    metrics.result_json_bytes = Some(
                        serde_json::to_vec(&value)
                            .expect("response serializes")
                            .len(),
                    );
                    Some(value)
                }
                Err(error) => {
                    self.failure("native result response", error);
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
            let Some(response) = self.request_result(&result.request, metrics).await else {
                return WorkStep::Failed;
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
            let finished = crate::commands::work::finish_result(
                &session,
                &self.config,
                &self.config_revision,
                id,
                result,
                reply,
                self.supervisor.read_control(),
            );
            metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
            match finished {
                Ok(crate::commands::work::ResultFinish::NeedsAction(reason)) => {
                    self.result.work_telemetry.retained_progress.action_receipts += 1;
                    self.record(
                        "native finalization action",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    WorkStep::Deferred(reason)
                }
                Ok(crate::commands::work::ResultFinish::Staged) => {
                    metrics.outcome = WorkOutcome::Staged;
                    self.record(
                        "bounded work completion",
                        StageState::Completed,
                        Some(id.into()),
                    );
                    WorkStep::Staged
                }
                Ok(crate::commands::work::ResultFinish::DiscoveryRecorded) => {
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
                    self.queue_replan = true;
                    WorkStep::Replan
                }
                Err(error) => {
                    self.failure("native finalization", error);
                    WorkStep::Failed
                }
            }
        }

        async fn selected_result_step(&mut self, inputs: &[serde_json::Value]) -> WorkStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
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
            let Some(value) = self.request_result(&request, &mut metrics).await else {
                self.record_work_step(metrics);
                return WorkStep::Failed;
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
            for (input, reply) in inputs.iter().zip(replies) {
                match crate::commands::work::generation::finish_selected(
                    &session,
                    &self.config,
                    &self.config_revision,
                    &request,
                    input,
                    reply,
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
                    Ok(crate::commands::work::generation::SelectedFinish::NeedsAction(reason)) => {
                        self.result.work_telemetry.retained_progress.action_receipts += 1;
                        deferred = true;
                        self.record("native selected action", StageState::Skipped, Some(reason));
                    }
                    Ok(crate::commands::work::generation::SelectedFinish::Staged) => {
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
                        self.record("expanded extraction work", StageState::Skipped, Some("selected scope changed; old semantic reply was not adopted; preparing fresh work".into()));
                    }
                    Err(error) => {
                        failed = true;
                        self.failure("native selected result", error);
                    }
                }
            }
            metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
            if staged {
                metrics.outcome = WorkOutcome::Staged;
            }
            self.record_work_step(metrics);
            if failed {
                WorkStep::Failed
            } else if recovered {
                self.queue_admitted.clear();
                self.queue_turn = None;
                self.queue_replan = true;
                WorkStep::Replan
            } else if deferred {
                WorkStep::Deferred("selected tasks require explicit actions".into())
            } else {
                WorkStep::Staged
            }
        }

        async fn whole_section_step(&mut self, task: &lk_queue::QueueTask) -> WorkStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
            let started = Instant::now();
            let prepared = crate::commands::work::section::prepare(
                &self.root,
                &self.config,
                &self.config_revision,
                &task.task_id,
            );
            metrics.preparation_duration_ms = Some(started.elapsed().as_millis() as u64);
            let section = match prepared {
                Ok(crate::commands::work::section::Preparation::Ready(section)) => section,
                Ok(crate::commands::work::section::Preparation::NeedsAction(reason)) => {
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
            let Some(reply) = self
                .request_observation(section.request.clone(), metrics)
                .await
            else {
                return WorkStep::Failed;
            };
            let (response, bytes) = match execution::finalization::decode(
                reply.engine,
                &reply.bytes,
                section.max_response_bytes,
            ) {
                Ok(response) => response,
                Err(error) => {
                    self.failure("whole-section response", error);
                    return WorkStep::Failed;
                }
            };
            metrics.result_json_bytes = Some(bytes);
            let started = Instant::now();
            let Some((session, _)) = self.mutation_session("whole-section publication").await
            else {
                if self.result.cancelled {
                    metrics.outcome = WorkOutcome::Cancelled;
                }
                return WorkStep::Failed;
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
            let Some(reply) = self
                .request_observation(section.request.clone(), metrics)
                .await
            else {
                return WorkStep::Failed;
            };
            let (response, bytes) = match execution::finalization::decode(
                reply.engine,
                &reply.bytes,
                section.max_response_bytes,
            ) {
                Ok(decoded) => decoded,
                Err(error) => {
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
                    Some(reason),
                ),
            };
            metrics.revalidation_record_duration_ms = Some(started_at.elapsed().as_millis() as u64);
            match finished {
                Ok(()) => match action {
                    Some(reason) => {
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
                Err(error) => {
                    self.failure("bounded section finalization", error);
                    WorkStep::Failed
                }
            }
        }

        async fn bounded_step(&mut self, id: &str, skill: &str) -> WorkStep {
            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
            metrics.work_id = Some(id.into());
            match self.bounded_observation_step(id).await {
                ObservationStep::Recorded => return WorkStep::Pending,
                ObservationStep::Deferred(reason) => return WorkStep::Deferred(reason),
                ObservationStep::Failed => return WorkStep::Failed,
                ObservationStep::Complete {
                    root_node,
                    preparation_duration_ms,
                } => {
                    metrics.node_id = Some(root_node);
                    metrics.preparation_duration_ms = Some(preparation_duration_ms);
                }
            }
            self.bounded_finish(id, skill, metrics, false).await
        }

        async fn bounded_finish(
            &mut self,
            id: &str,
            skill: &str,
            mut metrics: WorkStepMetrics,
            frontier: bool,
        ) -> WorkStep {
            let started_at = Instant::now();
            let checkpoint = match crate::commands::work::prepare_native_finalization(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
                frontier,
                self.supervisor.read_control(),
            ) {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    *metrics
                        .preparation_duration_ms
                        .as_mut()
                        .expect("measured preparation") += started_at.elapsed().as_millis() as u64;
                    self.record_work_step(metrics);
                    self.failure("bounded work completion", error);
                    return WorkStep::Failed;
                }
            };
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
                    self.queue_replan = true;
                    return WorkStep::Failed;
                }
            };
            match section {
                crate::commands::work::FinalizationPreparation::Interactive => {}
                crate::commands::work::FinalizationPreparation::Result(result) => {
                    let result = self.finalize_result(id, *result, &mut metrics).await;
                    self.record_work_step(metrics);
                    return result;
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
                    return WorkStep::Deferred(reason);
                }
                crate::commands::work::FinalizationPreparation::NeedsAction(reason) => {
                    self.record(
                        "bounded section action",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    self.record_work_step(metrics);
                    return WorkStep::Deferred(reason);
                }
                crate::commands::work::FinalizationPreparation::Ready(section) => {
                    let result = self.finalize_section(id, *section, &mut metrics).await;
                    self.record_work_step(metrics);
                    return result;
                }
            };
            let operation = format!(
                "bounded finish {id}. Start at the bounded-work reference's Finalize section. Read the complete lore work result for this exact audit work using bounded continuation reads when necessary. Preserve all attributed questions and blockers. Perform only decision-relevant targeted reads or review/repair actions; changed scope requires replanning. Submit with lore work finish. Leave publication/global checks to the native controller."
            );
            let calls_before = self.result.semantic_calls;
            let duration_before = self.result.semantic_duration_ms;
            let completed = self.consumer(skill, &operation, None).await;
            if self.result.semantic_calls > calls_before {
                metrics.agent_duration_ms =
                    Some(self.result.semantic_duration_ms - duration_before);
            }
            if !completed {
                metrics.outcome = if self.result.cancelled {
                    WorkOutcome::Cancelled
                } else if self.configuration_failed {
                    WorkOutcome::Rejected
                } else {
                    WorkOutcome::Failed
                };
                self.record_work_step(metrics);
                return WorkStep::Failed;
            }
            let started_at = Instant::now();
            let verified = crate::commands::work::verify_finished(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
            );
            *metrics
                .verification_duration_ms
                .as_mut()
                .expect("measured check") += started_at.elapsed().as_millis() as u64;
            let result = match verified {
                Ok(()) => {
                    metrics.outcome = WorkOutcome::Staged;
                    self.record(
                        "bounded work completion",
                        StageState::Completed,
                        Some(id.into()),
                    );
                    WorkStep::Staged
                }
                Err(error) => {
                    self.failure("bounded work completion", error);
                    WorkStep::Failed
                }
            };
            self.record_work_step(metrics);
            result
        }

        fn prepare_queue_screenings(
            &mut self,
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
                    &self.root,
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
                if expected.identity != format!("screening:{}", before.selection_id)
                    || before.complete
                {
                    self.queue_replan = true;
                    return Ok(None);
                }
                if let Some(reason) = before.blocked_reason {
                    self.queue_replan = true;
                    self.record("candidate screening", StageState::Failed, Some(reason));
                    return Ok(None);
                }
                let request = before.request.ok_or_else(|| {
                    miette::miette!(
                        "{} deferred and {} unknown candidate decisions remain; work retained",
                        before.deferred,
                        before.unknown
                    )
                })?;
                let task_id = match &jobs[target] {
                    QueueDispatch::Screening { task, .. } => task.task_id.clone(),
                    _ => unreachable!(),
                };
                let mut metrics = WorkStepMetrics::new(WorkRole::Screening);
                metrics.selection_id = Some(before.selection_id);
                metrics.batch_id = request["batch_id"].as_str().map(str::to_owned);
                metrics.preparation_from_queue_plan = reuse;
                metrics.preparation_duration_ms = (index == 0 && !reuse).then_some(duration_ms);
                metrics.request_json_bytes = Some(
                    serde_json::to_vec(&execution::observation::screening_request(&request)?)
                        .expect("native request serializes")
                        .len(),
                );
                let contract_revision =
                    execution::observation::screening_contract_revision(&request)?;
                turns.push(ScreeningAttempt {
                    task_id,
                    completed_batches: before.completed_batches,
                    batches: before.batches,
                    request,
                    contract_revision,
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
                Err(error) => return self.failure("semantic work budget", error),
            };
            let limit = budget
                .max_parallel
                .min(budget.max_calls.saturating_sub(self.result.semantic_calls));
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
            let screenings = match self.prepare_queue_screenings(&screen_targets, jobs) {
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
                    &self.root,
                    &self.config,
                    &self.config_revision,
                    &demands,
                    self.supervisor.read_control(),
                ) {
                    Ok(captured) => captured,
                    Err(error) => return self.failure("bounded work progress", error),
                };
                self.result
                    .work_telemetry
                    .evidence_preparation_io
                    .accumulate(captured.discovery_io);
                self.result.work_telemetry.evidence_preparation_duration_ms += captured.duration_ms;
                preparation_duration_ms = Some(captured.duration_ms);
                for ((id, _), result) in demands.into_iter().zip(captured.turns) {
                    let (progress, requests) = match result {
                        Ok(captured) => captured,
                        Err(error)
                            if error
                                .downcast_ref::<crate::commands::work::state::InputCapacity>()
                                .is_some() =>
                        {
                            self.deferred("bounded work capacity", error);
                            self.queue_replan = true;
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
                            self.queue_replan = true;
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
                        return self
                            .failure("bounded work progress", "incomplete work has no ready node");
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
                        match crate::commands::work::inspection_action(&self.root, id, &request) {
                            Ok(Some(reason)) => {
                                self.inspection_deferred.insert(id.clone(), reason.clone());
                                self.deferred("bounded work action", reason);
                                self.queue_replan = true;
                                self.queue_admitted.clear();
                                self.record_uninvoked_queue_reads(turns, WorkOutcome::Rejected);
                                return true;
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
            if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                self.record_uninvoked_queue_reads(turns, WorkOutcome::Rejected);
                return false;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                self.record_uninvoked_queue_reads(turns, WorkOutcome::Failed);
                return self.failure(
                    "bounded queue receipt",
                    "semantic execution is disabled by llm.provider",
                );
            }
            if let Err(error) = self.prepare_agent().await {
                self.record_uninvoked_queue_reads(turns, WorkOutcome::Failed);
                return if self.result.cancelled {
                    self.record(
                        "bounded queue receipt",
                        StageState::Cancelled,
                        Some(error.to_string()),
                    )
                } else {
                    self.failure("bounded queue receipt", error)
                };
            }
            let timeout = self.selected.as_ref().expect("prepared agent").timeout();
            let mut requests = Vec::new();
            let mut directories = Vec::new();
            let mut engines = Vec::new();
            for (_, turn) in &mut turns {
                let (invocation, limit) =
                    match self.observation_invocation(turn.request(), turn.role(), budget) {
                        Ok(invocation) => invocation,
                        Err(error) => {
                            self.record_uninvoked_queue_reads(turns, WorkOutcome::Failed);
                            return self.failure("bounded queue receipt", error);
                        }
                    };
                turn.metrics_mut().request_json_bytes = Some(invocation.request_json_bytes);
                engines.push(invocation.engine);
                let (request, directory) = invocation.into_request(timeout, limit);
                requests.push(request);
                directories.push(directory);
            }
            {
                let Some((session, waited)) = self.mutation_session("queue scheduling").await
                else {
                    self.record_uninvoked_queue_reads(turns, WorkOutcome::Failed);
                    return false;
                };
                if waited {
                    self.queue_admitted.clear();
                    self.queue_replan = true;
                    self.record("queue input refresh",StageState::Skipped,Some("writer access became available; preparing current inputs before invocation".into()));
                    self.record_uninvoked_queue_reads(turns, WorkOutcome::Rejected);
                    return true;
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
                    let targets = admitted
                        .iter()
                        .map(|binding| binding.target.clone())
                        .collect::<Vec<_>>();
                    if let Err(error) = scheduler::reserve_in_session(
                        &session,
                        scheduler::Stage::Queue,
                        keys,
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
            }
            eprintln!(
                "{}",
                serde_json::json!({
                    "type":"lore.queue-read-wave",
                    "request_count":requests.len(),
                    "evidence_requests":turns.iter().filter(|(_,turn)|matches!(turn,QueueReadTurn::Evidence(_))).count(),
                    "screening_requests":turns.iter().filter(|(_,turn)|matches!(turn,QueueReadTurn::Screening(_))).count(),
                    "max_parallel":budget.max_parallel,
                    "remaining_calls":budget.max_calls.saturating_sub(self.result.semantic_calls),
                    "elapsed_job_ms":self.started_at.elapsed().as_millis(),
                })
            );
            self.result.semantic_calls += requests.len();
            let attempts = self.supervisor.run_requests(requests).await;
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
            let screening_ok = if screenings.is_empty() {
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
            let observation_ok = if observations.is_empty() {
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
            screening_ok && observation_ok
        }

        async fn settle_screening_attempts(
            &mut self,
            turns: Vec<ScreeningAttempt>,
            attempts: Vec<execution::ChildAttempt>,
            engines: Vec<lk_core::execution::AgentEngine>,
            result_limit: usize,
            cancelled: bool,
        ) -> bool {
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
            for ((turn, attempt), engine) in turns.into_iter().zip(attempts).zip(engines) {
                let ScreeningAttempt {
                    task_id,
                    completed_batches,
                    batches,
                    request,
                    contract_revision,
                    metrics: mut step,
                } = turn;
                step.agent_duration_ms = Some(attempt.duration_ms);
                self.result.semantic_duration_ms += attempt.duration_ms;
                let decoded = if cancelled || !current {
                    step.outcome = if cancelled {
                        WorkOutcome::Cancelled
                    } else {
                        WorkOutcome::Rejected
                    };
                    None
                } else {
                    match attempt.result {
                        Ok(child) if child.success => {
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
                                        Ok(output) => Some(crate::commands::work::ScreeningResponse::Decisions(output)),
                                        Err(error) if error.downcast_ref::<crate::commands::work::ScreeningRejection>().is_some() => {
                                            Some(crate::commands::work::ScreeningResponse::Rejected {
                                                request: request.clone(),
                                                contract_revision,
                                                rejection: error.downcast_ref::<crate::commands::work::ScreeningRejection>().expect("typed rejection").clone(),
                                            })
                                        }
                                        Err(error) => {
                                            self.failure("candidate screening", error);
                                            None
                                        }
                                    }
                                }
                                Err(error) => {
                                    self.failure("candidate screening", error);
                                    None
                                }
                            }
                        }
                        Ok(child) => {
                            step.outcome = WorkOutcome::Failed;
                            self.result.halted = true;
                            self.record("candidate screening", StageState::Failed, child.detail);
                            None
                        }
                        Err(error) => {
                            step.outcome = WorkOutcome::Failed;
                            self.failure("candidate screening", error);
                            None
                        }
                    }
                };
                if let Some(output) = decoded {
                    replies.push(ScreeningReply {
                        task_id,
                        output,
                        completed_batches,
                        batches,
                        metric_index: metrics.len(),
                    });
                } else {
                    accepted = false;
                }
                metrics.push(step);
            }
            if !replies.is_empty() {
                let started_at = Instant::now();
                let recorded = if let Some((session, _)) =
                    self.recording_session("candidate screening").await
                {
                    crate::commands::work::record_screenings(
                        &session,
                        &self.config,
                        &self.config_revision,
                        replies
                            .iter()
                            .map(|reply| (reply.task_id.as_str(), reply.output.clone()))
                            .collect(),
                        self.supervisor.read_control(),
                    )
                } else {
                    Err(miette::miette!(
                        "candidate screening could not acquire writer access"
                    ))
                };
                let total_duration_ms = started_at.elapsed().as_millis() as u64;
                match recorded {
                    Ok(recorded) => {
                        self.result
                            .work_telemetry
                            .screening_revalidation_duration_ms +=
                            recorded.revalidation_duration_ms;
                        self.result
                            .work_telemetry
                            .screening_revalidation_io
                            .accumulate(recorded.discovery_io);
                        let recorded = recorded.records;
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
                                Ok(crate::commands::work::ScreeningRecordOutcome::Deferred(
                                    reason,
                                )) => {
                                    self.result.work_telemetry.retained_progress.action_receipts +=
                                        1;
                                    step.outcome = WorkOutcome::Rejected;
                                    self.queue_replan = true;
                                    self.queue_admitted.clear();
                                    self.record(
                                        "candidate screening",
                                        StageState::Failed,
                                        Some(reason),
                                    );
                                }
                                Ok(crate::commands::work::ScreeningRecordOutcome::Recorded(
                                    completed,
                                )) if completed > reply.completed_batches => {
                                    step.outcome = WorkOutcome::Recorded;
                                    self.record("candidate screening", StageState::Completed, Some(format!("{completed}/{} screening batches recorded; card decisions are not full-body assessments", reply.batches)));
                                    self.queue_replan |= completed == reply.batches;
                                }
                                Ok(_) => {
                                    accepted = false;
                                    self.failure("candidate screening", "worker did not advance the selected screening batch; work retained");
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
            accepted
        }

        async fn queue_dispatches(
            &mut self,
            blocked: &std::collections::BTreeMap<String, String>,
        ) -> miette::Result<QueuePlan> {
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
            let (session, _) = self
                .mutation_session("queue planning")
                .await
                .ok_or_else(|| miette::miette!("queue planning did not acquire writer access"))?;
            let mut tasks = crate::commands::queue::current_tasks(session.root())?;
            tasks.sort_by(|left, right| {
                (left.created_at, &left.task_id).cmp(&(right.created_at, &right.task_id))
            });
            let mut owners = std::collections::BTreeMap::new();
            for task in tasks {
                let owner = lk_vault::page_path(session.root(), Path::new(&task.target.vault_path))
                    .and_then(|path| path.canonicalize())
                    .map_err(|error| miette::miette!("queue target: {error}"))?;
                owners.entry(owner).or_insert(task);
            }
            let mut readiness = owners
                .values()
                .any(|task| task.kind == lk_queue::TaskKind::SynthesizeConcept)
                .then(|| super::super::queue::readiness::InputReadiness::load(session.root()))
                .transpose()?;
            let budget = self
                .config
                .execution
                .effective_work()
                .map_err(|error| miette::miette!("{error}"))?;
            let mut deferred = std::collections::BTreeMap::new();
            let mut targets = std::collections::BTreeMap::new();
            let rejected = crate::commands::queue::rejections::blocked_tasks(
                session.root(),
                &self.config,
                &self.config_revision,
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
                if task.kind == lk_queue::TaskKind::SynthesizeConcept {
                    let reasons = readiness
                        .as_mut()
                        .expect("synthesis readiness was loaded")
                        .unfinished_synthesis(session.root(), &task)?;
                    if !reasons.is_empty() {
                        deferred.insert(target, reasons);
                        continue;
                    }
                }
                targets.insert(target, task);
            }
            let mut keys = targets.keys().cloned().collect();
            let mut jobs = std::collections::BTreeMap::new();
            if needs_application
                || !targets.is_empty() && self.result.semantic_calls >= budget.max_calls
            {
                return Ok(QueuePlan {
                    jobs,
                    deferred,
                    keys,
                    needs_application,
                });
            }
            let order = scheduler::order(session.root(), scheduler::Stage::Queue, &keys)?;
            let independent_limit = budget
                .max_parallel
                .min(budget.max_calls.saturating_sub(self.result.semantic_calls))
                .min(MAX_PREPARED_QUEUE_TARGETS);
            let mut next = 0;
            while next < order.len() {
                self.supervisor.read_control().check().into_diagnostic()?;
                let batch = is_interactive_batch_candidate(
                    targets[&order[next]].kind,
                    budget.extraction_context,
                );
                let limit = if batch && jobs.is_empty() {
                    MAX_PREPARED_QUEUE_TARGETS
                } else {
                    independent_limit.saturating_sub(jobs.len())
                };
                let count = order[next..]
                    .iter()
                    .take(limit)
                    .take_while(|target| {
                        is_interactive_batch_candidate(
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
                    if crate::commands::work::needs_discovery(
                        session.root(),
                        &self.config,
                        &self.config_revision,
                        task,
                    )? {
                        jobs.insert(
                            target.clone(),
                            QueueDispatch::Discovery { task: task.clone() },
                        );
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
                    let pages = lk_graph::scan::scan_vault(
                        session.root(),
                        self.config.graph.scope.follow_links,
                    )
                    .map_err(|error| miette::miette!("{error}"))?;
                    lk_graph::audit::load_selected_inputs(
                        session.root(),
                        &self.config.vault.dirs,
                        &pages,
                        &synthesis_paths,
                    )
                    .map_err(|error| miette::miette!("{error}"))?
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
                    .map(|(task, work)| (task.target.vault_path.clone(), (task, work)))
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
                            match crate::commands::work::section::prepare(
                                session.root(),
                                &self.config,
                                &self.config_revision,
                                &task.task_id,
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
                                *native.expect("interactive task has a prepared native input");
                            if individual {
                                flush(&mut batch, &mut jobs);
                            }
                            let mut inputs = batch
                                .iter()
                                .map(|(_, input)| input.clone())
                                .collect::<Vec<_>>();
                            inputs.push(input.clone());
                            let request = crate::commands::work::native::selected_request(
                                &self.config,
                                &inputs,
                            )?;
                            if serde_json::to_vec(&request)
                                .map_err(|error| miette::miette!("{error}"))?
                                .len()
                                > budget.max_input_bytes
                            {
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
                            if let Some(reason) = blocked
                                .get(&id)
                                .or_else(|| self.inspection_deferred.get(&id))
                            {
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
                                    selection_id: turn.selection_id.clone(),
                                    prepared: Some(turn),
                                    prepared_at_call: self.result.semantic_calls,
                                },
                            );
                        }
                    }
                }
                flush(&mut batch, &mut jobs);
                if !jobs.is_empty()
                    && (jobs.len() >= independent_limit
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
            let mut unfinished = false;
            for task in tasks {
                if crate::commands::queue::tasks_completed(root, std::slice::from_ref(task))? {
                    continue;
                }
                unfinished = true;
                if task.kind != lk_queue::TaskKind::ExtractConcepts {
                    return Ok(false);
                }
                let current = crate::commands::queue::current_task(root, &task.task_id)?;
                if current.kind != task.kind
                    || current.cache_hash != task.cache_hash
                    || current.target != task.target
                    || current.input != task.input
                    || current.created_at != task.created_at
                    || !crate::commands::work::has_current_discovery(root, &current)?
                    || !matches!(
                        crate::commands::work::prepare_task(
                            &session,
                            &self.config,
                            &self.config_revision,
                            &current.task_id,
                        )?,
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
            let mut blocked = std::collections::BTreeMap::<String, String>::new();
            'planning: loop {
                self.queue_replan = false;
                if self.result.halted || self.result.cancelled || !self.configuration_is_current() {
                    return false;
                }
                self.queue_turn = None;
                self.queue_admitted.clear();
                let started_at = Instant::now();
                let acquisition_before = self.result.mutation_access.duration_ms;
                let before = self.result.queue_planning;
                self.result.queue_planning.attempts += 1;
                eprintln!(
                    "{}",
                    serde_json::json!({"type":"lore.queue-planning","state":"started","attempt":self.result.queue_planning.attempts,"elapsed_job_ms":self.started_at.elapsed().as_millis()})
                );
                let planned = self.queue_dispatches(&blocked).await;
                let acquisition_ms = self.result.mutation_access.duration_ms - acquisition_before;
                self.result.queue_planning.duration_ms +=
                    (started_at.elapsed().as_millis() as u64).saturating_sub(acquisition_ms);
                eprintln!(
                    "{}",
                    serde_json::json!({"type":"lore.queue-planning","state":if planned.is_ok() { "completed" } else { "failed" },"attempt":self.result.queue_planning.attempts,"duration_ms":self.result.queue_planning.duration_ms-before.duration_ms,"prepared_targets":self.result.queue_planning.prepared_targets-before.prepared_targets,"discovery_captures":self.result.queue_planning.discovery_captures-before.discovery_captures,"audit_captures":self.result.queue_planning.audit_captures-before.audit_captures,"elapsed_job_ms":self.started_at.elapsed().as_millis()})
                );
                let plan = match planned {
                    Ok(plan) => plan,
                    Err(_) if self.result.halted || self.result.cancelled => return false,
                    Err(error) => return self.failure("queue planning", error),
                };
                if plan.needs_application {
                    if !self.apply_queue("changed rejected-result validation").await {
                        return false;
                    }
                    continue;
                }
                let mut jobs = plan.jobs;
                if jobs.is_empty() {
                    if !plan.keys.is_empty() && !self.call_budget_available() {
                        return false;
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
                    let selected = match admitted {
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
                        if !self.queue_read_wave(&selected, &mut jobs, keys).await {
                            return false;
                        }
                        if self.queue_replan
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
                    self.queue_turn = Some(QueueTurn {
                        keys: keys.clone(),
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
                                WorkStep::Failed if self.queue_replan => continue 'planning,
                                WorkStep::Failed => return false,
                                WorkStep::Pending => {
                                    unreachable!("single section has no evidence nodes")
                                }
                            }
                        }
                        QueueDispatch::Discovery { task } => {
                            let directory = match tempfile::Builder::new()
                                .prefix("lorekeeper-discovery-")
                                .tempdir()
                            {
                                Ok(directory) => directory,
                                Err(error) => {
                                    return self.failure("extraction discovery selection", error);
                                }
                            };
                            let path = directory.path().join("tasks.json");
                            if let Err(error) = std::fs::write(
                                &path,
                                serde_json::to_vec(&[&task.task_id]).expect("task ids serialize"),
                            ) {
                                return self.failure("extraction discovery selection", error);
                            }
                            let selection = match serde_json::to_string(&path) {
                                Ok(selection) => selection,
                                Err(error) => {
                                    return self.failure("extraction discovery selection", error);
                                }
                            };
                            if !self
                                .consumer(
                                    "lore-process",
                                    &format!("discover {selection}"),
                                    Some(directory.path()),
                                )
                                .await
                            {
                                if self.queue_replan {
                                    continue 'planning;
                                }
                                return false;
                            }
                            match crate::commands::work::has_current_discovery(&self.root, task) {
                                Ok(true) => {
                                    self.result.work_telemetry.retained_progress.discovery_recipes += 1;
                                    continue 'planning;
                                }
                                Ok(false) => return self.failure("extraction discovery receipt", "selected extraction has no current discovery recipe; unfinished work retained"),
                                Err(error) => return self.failure("extraction discovery receipt", error),
                            }
                        }
                        QueueDispatch::Finalization { id, .. } => {
                            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
                            metrics.work_id = Some(id.clone());
                            metrics.preparation_duration_ms = Some(0);
                            match self.bounded_finish(id, "lore-process", metrics, true).await {
                                WorkStep::Staged => {}
                                WorkStep::Deferred(reason) => {
                                    blocked.insert(id.clone(), reason);
                                    continue 'planning;
                                }
                                WorkStep::Replan => continue 'planning,
                                WorkStep::Failed if self.queue_replan => continue 'planning,
                                WorkStep::Failed => return false,
                                WorkStep::Pending => {
                                    unreachable!("frontier finalization has no pending inspection")
                                }
                            }
                        }
                        QueueDispatch::RootFinalization { id, .. } => {
                            match self.bounded_step(id, "lore-process").await {
                                WorkStep::Pending => continue,
                                WorkStep::Deferred(reason) => {
                                    blocked.insert(id.clone(), reason);
                                    continue 'planning;
                                }
                                WorkStep::Replan => continue 'planning,
                                WorkStep::Failed if self.queue_replan => continue 'planning,
                                WorkStep::Failed => return false,
                                WorkStep::Staged => {}
                            }
                        }
                        QueueDispatch::Partitioned { .. }
                        | QueueDispatch::Source { .. }
                        | QueueDispatch::Screening { .. } => {
                            unreachable!("read-only queue wave handled above")
                        }
                        QueueDispatch::WholeInput { tasks: _, inputs } => {
                            match self.selected_result_step(inputs).await {
                                WorkStep::Staged => {}
                                WorkStep::Deferred(_) => generation_deferred = true,
                                WorkStep::Replan => {
                                    generation_deferred = true;
                                }
                                WorkStep::Failed if self.queue_replan => continue 'planning,
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
                            .native(
                                "queue publication reconciliation",
                                &["graph", "backlinks-sync"],
                            )
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
                        match crate::commands::queue::tasks_completed(
                            &self.root,
                            std::slice::from_ref(task),
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
                            .native(
                                "queue publication reconciliation",
                                &["graph", "backlinks-sync"],
                            )
                            .await
                    {
                        return false;
                    }
                    if !unfinished.is_empty() {
                        let rejected = match crate::commands::queue::rejections::blocked_tasks(
                            &self.root,
                            &self.config,
                            &self.config_revision,
                        ) {
                            Ok(rejected) => rejected,
                            Err(error) => {
                                return self.failure("selected queue rejection", error);
                            }
                        };
                        if unfinished.iter().all(|task| {
                            rejected.contains_key(&(task.task_id.clone(), task.cache_hash.clone()))
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
                                Err(_) if self.result.halted || self.result.cancelled => {
                                    return false;
                                }
                                Err(error) => {
                                    return self.failure("selected queue receipt", error);
                                }
                            }
                        }
                        return self.failure("selected queue receipt", "selected tasks are not completed against their retained inputs; unfinished work retained");
                    }
                    break;
                }
            }
        }

        async fn drain(&mut self) -> bool {
            if self.result.halted || self.result.cancelled {
                return false;
            }
            let state = match crate::commands::queue::check::inspect(&self.root) {
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
                match crate::commands::queue::check::inspect(&self.root) {
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
                        "semantic processing is disabled by llm.provider",
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
                    self.drain_blocked = self.result.halted;
                    completed
                }
            }
        }

        async fn reconcile(&mut self) {
            for (name, args) in [
                ("schema", vec!["schema"]),
                ("queue apply", vec!["queue", "apply"]),
                ("backlinks-sync", vec!["graph", "backlinks-sync"]),
            ] {
                self.native(name, &args).await;
            }
            self.drain().await;
            for (name, args) in [
                ("queue apply", vec!["queue", "apply"]),
                ("backlinks-sync", vec!["graph", "backlinks-sync"]),
                ("wiki refresh", vec!["wiki", "refresh"]),
                ("graph lint", vec!["graph", "lint"]),
                ("queue completion", vec!["queue", "check"]),
            ] {
                self.native(name, &args).await;
            }
        }

        async fn inspect_halted_state(&mut self) {
            if !self.result.halted || self.result.cancelled || !self.configuration_is_current() {
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
                match self.supervisor.run(command, None, false).await {
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

        fn audit_plan(&mut self, paths: &[String]) -> miette::Result<lk_graph::audit::AuditPlan> {
            let pages =
                lk_graph::scan::scan_vault(&self.root, self.config.graph.scope.follow_links)
                    .map_err(|e| miette::miette!("{e}"))?;
            let mut readiness = InputAvailability::load(&self.root)?;
            let plan = lk_graph::audit::plan_selected_available_partitioned(
                &self.root,
                &self.config.vault.dirs,
                &pages,
                self.config.graph.audit.batch_size,
                paths,
                self.config.graph.audit.max_input_bytes,
                &mut |candidate| readiness.check(candidate),
            )
            .map_err(|e| miette::miette!("{e}"))?;
            self.result.audit_deferred = readiness.deferred().to_vec();
            self.result.audit_deferred_count = readiness.deferred_count();
            Ok(plan)
        }

        async fn admit_audit(&mut self) -> miette::Result<lk_graph::audit::AuditPlan> {
            let (session, _) = self
                .mutation_session("audit admission")
                .await
                .ok_or_else(|| miette::miette!("audit admission did not acquire writer access"))?;
            let pages =
                lk_graph::scan::scan_vault(&self.root, self.config.graph.scope.follow_links)
                    .map_err(|error| miette::miette!("{error}"))?;
            let _queue =
                super::super::queue::claim_writer(&self.root.join(".lorekeeper/queue"), false)?;
            let mut readiness = InputAvailability::load(&self.root)?;
            let plan = lk_graph::audit::admit_available_partitioned(
                &session,
                &self.config.vault.dirs,
                &pages,
                self.config.graph.audit.batch_size,
                self.config.graph.audit.max_input_bytes,
                &mut |candidate| readiness.check(candidate),
                |candidates, mutation| {
                    let mut selected = InputAvailability::load(&self.root)
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
            )
            .map_err(|error| miette::miette!("{error}"))?;
            self.result.audit_deferred = readiness.deferred().to_vec();
            self.result.audit_deferred_count = readiness.deferred_count();
            Ok(plan)
        }

        fn verify_audit(
            &self,
            batch: &[lk_graph::audit::AuditCandidate],
        ) -> miette::Result<lk_graph::audit::AuditVerification> {
            let pages =
                lk_graph::scan::scan_vault(&self.root, self.config.graph.scope.follow_links)
                    .map_err(|e| miette::miette!("{e}"))?;
            let workflow = self
                .lease
                .as_ref()
                .ok_or_else(|| miette::miette!("audit has no workflow lease"))?;
            let verified = lk_graph::audit::verify_batch_with_guard(
                &self.root,
                &self.config.vault.dirs,
                &pages,
                batch,
                workflow.id(),
                self.config.graph.audit.max_input_bytes,
                |assessment, mutation| {
                    crate::commands::work::guard_audit(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        assessment,
                        mutation,
                    )
                    .map(|completed| {
                        if completed {
                            lk_graph::audit::AuditReadCoverage::CompletedWork
                        } else {
                            lk_graph::audit::AuditReadCoverage::WholeInput
                        }
                    })
                    .map_err(|error| lk_graph::GraphError::Io(error.to_string()))
                },
            )
            .map_err(|e| miette::miette!("{e}"))?;
            let paths = verified
                .completed
                .iter()
                .map(|completed| completed.assessed_path.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let selected = lk_graph::audit::plan_selected_partitioned(
                &self.root,
                &self.config.vault.dirs,
                &pages,
                paths.len(),
                &paths,
                self.config.graph.audit.max_input_bytes,
            )
            .map_err(|error| miette::miette!("{error}"))?;
            let mut readiness = super::super::queue::readiness::InputReadiness::load(&self.root)?;
            for candidate in &selected.candidates {
                readiness.require_ready(&self.root, candidate)?;
            }
            Ok(verified)
        }

        fn inspect_review_health(&mut self) -> bool {
            if self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            self.result.review_health = None;
            let reports = match lk_vault::list_reviews(&self.root, &self.config.vault.dirs) {
                Ok(reports) => reports,
                Err(error) => return self.failure("knowledge review health", error),
            };
            if !self.configuration_is_current() {
                return false;
            }
            let mut health = ReviewHealth::default();
            for report in reports {
                match report.effective_state {
                    lk_vault::ReviewEffectiveState::Open => health.open += 1,
                    lk_vault::ReviewEffectiveState::Resolved => health.resolved += 1,
                    lk_vault::ReviewEffectiveState::NeedsReassessment => {
                        health.needs_reassessment += 1;
                    }
                    lk_vault::ReviewEffectiveState::Invalid => health.invalid += 1,
                }
            }
            let detail = format!(
                "open={}, resolved={}, needs_reassessment={}, invalid={}; inspect `lore wiki reviews` for details",
                health.open, health.resolved, health.needs_reassessment, health.invalid,
            );
            let valid = health.invalid == 0;
            self.result.halted |= !valid;
            self.result.review_health = Some(health);
            self.record(
                "knowledge review health",
                if valid {
                    StageState::Completed
                } else {
                    StageState::Failed
                },
                Some(detail),
            )
        }

        async fn knowledge(&mut self) {
            self.reconcile_audit_inputs().await;
            if self.result.halted || self.result.cancelled {
                return;
            }
            self.audit().await;
            self.reconcile().await;
            if !self.inspect_review_health() {
                return;
            }
            if let Some(batch) = self.audit_batch.take() {
                match self.verify_audit(&batch) {
                    Ok(verified) => {
                        self.result.audit = Some(verified.completed);
                        if let Some(progress) = &mut self.result.audit_progress {
                            progress.after = Some(verified.coverage);
                        }
                        self.record("audit completion", StageState::Completed, None);
                    }
                    Err(error) => {
                        self.failure("audit completion", error);
                    }
                }
            } else if self.result.audit.as_ref().is_some_and(Vec::is_empty) {
                match self.audit_plan(&[]) {
                    Ok(plan) => {
                        if let Some(progress) = &mut self.result.audit_progress {
                            progress.after = Some(plan.coverage());
                        }
                    }
                    Err(error) => {
                        self.failure("audit final coverage", error);
                    }
                }
            }
        }

        async fn reconcile_audit_inputs(&mut self) {
            for (name, args) in [
                ("schema", vec!["schema"]),
                ("queue apply", vec!["queue", "apply"]),
                ("backlinks-sync", vec!["graph", "backlinks-sync"]),
                ("wiki refresh", vec!["wiki", "refresh"]),
                ("graph lint", vec!["graph", "lint"]),
            ] {
                if !self.native(name, &args).await {
                    return;
                }
            }
        }

        async fn audit(&mut self) {
            if !self.inspect_review_health() {
                return;
            }
            let mut plan = match self.admit_audit().await {
                Ok(plan) => plan,
                Err(_) if self.result.halted || self.result.cancelled => return,
                Err(error) => {
                    self.failure("audit planning", error);
                    return;
                }
            };
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
                if !self.result.audit_deferred.is_empty() {
                    self.deferred("audit dependencies", "pending subjects have unfinished semantic inputs; retained for later assessment");
                    return;
                }
                self.result.audit = Some(Vec::new());
                if plan.blocked_count > 0 {
                    return;
                }
                if let Some(progress) = &mut self.result.audit_progress {
                    progress.after = Some(progress.before.clone());
                }
                self.record(
                    "knowledge audit",
                    StageState::Skipped,
                    Some("no audit candidates".into()),
                );
                return;
            }
            let paths = plan
                .candidates
                .iter()
                .map(|candidate| candidate.path.clone())
                .collect::<Vec<_>>();
            let missing = plan
                .candidates
                .iter()
                .filter(|candidate| candidate.discovery.is_none())
                .map(|candidate| candidate.path.as_str())
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                let missing = serde_json::to_string(&missing).expect("string paths serialize");
                if !self.consumer("lore-wiki", &format!("discover. Plan related-document search only for these exact audit subjects: {missing}. Read their names, scope and outline, choose explicit search queries and record each through wiki audit-discover. Do not audit, repair, synthesize or record completed assessments in this discovery session."), None).await {
                    return;
                }
                plan = match self.audit_plan(&paths) {
                    Ok(plan) => plan,
                    Err(error) => {
                        self.failure("discovery planning", error);
                        return;
                    }
                };
                if self.result.audit_deferred_count > 0 {
                    self.deferred(
                        "audit discovery dependencies",
                        format!(
                            "{} selected audit subjects have unfinished discovered inputs; retained for later assessment",
                            self.result.audit_deferred_count,
                        ),
                    );
                    if plan.candidates.is_empty() {
                        return;
                    }
                }
                if plan
                    .candidates
                    .iter()
                    .any(|candidate| candidate.discovery.is_none())
                {
                    self.failure(
                        "related-document discovery",
                        "selected subjects lack current recorded search queries",
                    );
                    return;
                }
            }
            let selection = serde_json::to_string(
                &plan
                    .candidates
                    .iter()
                    .map(|candidate| &candidate.path)
                    .collect::<Vec<_>>(),
            )
            .expect("string paths serialize");
            let mut bounded = plan.input.ensure_ready().is_err();
            for candidate in &plan.candidates {
                match crate::commands::work::audit_needs_work(
                    &self.root,
                    &self.config,
                    &self.config_revision,
                    candidate,
                ) {
                    Ok(required) => bounded |= required,
                    Err(error) => {
                        self.failure("audit request planning", error);
                        return;
                    }
                }
            }
            let mut deferred_subjects = std::collections::BTreeSet::new();
            if bounded {
                let mut jobs = std::collections::BTreeMap::new();
                for candidate in &plan.candidates {
                    let path = &candidate.path;
                    let Some((session, _)) = self.mutation_session("bounded audit planning").await
                    else {
                        return;
                    };
                    let work = match crate::commands::work::prepare_audit(
                        &session,
                        &self.config,
                        &self.config_revision,
                        candidate,
                    ) {
                        Ok(id) => id,
                        Err(error) => {
                            self.failure("bounded audit planning", error);
                            return;
                        }
                    };
                    jobs.insert(path.clone(), work);
                }
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
                        return;
                    }
                    let keys = jobs.keys().cloned().collect();
                    let Some((session, _)) = self.mutation_session("audit scheduling").await else {
                        return;
                    };
                    let selected = match scheduler::select_in_session(
                        &session,
                        scheduler::Stage::Audit,
                        &keys,
                    ) {
                        Ok(Some(selected)) => selected,
                        Ok(None) => break,
                        Err(error) => {
                            self.failure("audit scheduling", error);
                            return;
                        }
                    };
                    drop(session);
                    let work = &jobs[&selected];
                    if let Some(completed) = self.bounded_audit_wave(&selected, &jobs, &keys).await
                    {
                        if !completed {
                            return;
                        }
                        continue;
                    }
                    match self.bounded_step(work, "lore-wiki").await {
                        WorkStep::Pending => continue,
                        WorkStep::Deferred(reason) => {
                            self.deferred("bounded audit action", reason);
                            deferred_subjects.insert(selected.clone());
                            jobs.remove(&selected);
                            continue;
                        }
                        WorkStep::Failed => return,
                        WorkStep::Replan => {
                            unreachable!("audit cannot request extraction discovery")
                        }
                        WorkStep::Staged => {}
                    }
                    let assessment = self
                        .root
                        .join(".lorekeeper/work")
                        .join(work)
                        .join("assessment.json");
                    let assessment = match assessment.to_str() {
                        Some(path) => path.to_owned(),
                        None => {
                            self.failure(
                                "bounded audit publication",
                                "assessment path is not UTF-8",
                            );
                            return;
                        }
                    };
                    if !self
                        .native(
                            "bounded audit publication",
                            &["wiki", "audit-record", "--file", &assessment],
                        )
                        .await
                    {
                        return;
                    }
                    self.audit_batch.get_or_insert_with(Vec::new).push(
                        plan.candidates
                            .iter()
                            .find(|candidate| candidate.path == selected)
                            .expect("prepared audit belongs to the selected batch")
                            .clone(),
                    );
                    jobs.remove(&selected);
                }
            } else {
                self.consumer("lore-wiki", &format!("maintain. The controller selected this exact batch: {selection}. Start with audit-plan --path for these addresses; do not select another batch. Use bounded work for any subject whose work envelope requires partitioning."), None).await;
            }
            let assessed = plan
                .candidates
                .into_iter()
                .filter(|candidate| !deferred_subjects.contains(&candidate.path))
                .collect::<Vec<_>>();
            if !deferred_subjects.is_empty() {
                self.deferred("audit inspection dependencies", format!("{} selected audit subjects require explicit processing action; no assessment was recorded for them", deferred_subjects.len()));
            }
            if assessed.is_empty() {
                self.result.audit = Some(Vec::new());
            } else {
                self.audit_batch = Some(assessed);
            }
        }
    }

    pub(super) async fn run(
        opts: &GlobalOptions,
        installation: &lk_dist::Installation,
        operation: Operation,
        profile: Option<String>,
        batches: usize,
        wait: bool,
        json: bool,
    ) -> miette::Result<()> {
        let started_at = Instant::now();
        if batches == 0 || batches != 1 && matches!(operation, Operation::Daily) {
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
                .map_err(|e| miette::miette!("{e}"))?
                .0
                .to_owned();
            config
                .execution
                .select_profile(&name)
                .map_err(|e| miette::miette!("{e}"))?;
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
        let mut supervisor = execution::Supervisor::new()?;
        let mut outcomes = Vec::new();
        let mut stop_reason = CampaignStop::BatchLimit;
        let mut campaign_root = None;
        for _ in 0..batches {
            let result = execute(
                &options,
                installation,
                (config.clone(), config_revision.clone()),
                operation,
                wait,
                &mut supervisor,
                &mut campaign_root,
            )
            .await;
            stop_reason = result.campaign_stop();
            outcomes.push(result);
            if stop_reason != CampaignStop::BatchLimit {
                break;
            }
        }
        let ok = if matches!(operation, Operation::Process) && batches > 1 {
            stop_reason == CampaignStop::Settled
        } else {
            outcomes.iter().all(|result| result.ok)
        };
        if json {
            let value = if batches == 1 {
                serde_json::to_value(&outcomes[0])
            } else {
                Ok(serde_json::json!({
                    "operation":operation,"ok":ok,"requested_batches":batches,
                    "stop_reason":stop_reason,"runs":outcomes,
                    "duration_ms":started_at.elapsed().as_millis() as u64,
                }))
            }
            .map_err(|error| miette::miette!("{error}"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|error| miette::miette!("{error}"))?
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
        supervisor: &mut execution::Supervisor,
        campaign_root: &mut Option<workflow::VaultRootPin>,
    ) -> JobResult {
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
            lease: None,
            selected: None,
            project_protection: None,
            drain_blocked: false,
            inspection_deferred: std::collections::BTreeMap::new(),
            audit_batch: None,
            queue_turn: None,
            queue_replan: false,
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
            match operation {
                Operation::Daily => {
                    runner.drain().await;
                    if board {
                        runner.native("task propose", &["task", "propose"]).await;
                    }
                    runner.reconcile().await;
                    runner
                        .native("knowledge reviews", &["wiki", "reviews", "--check"])
                        .await;
                    runner.resume_periods().await;
                    runner.reconcile().await;
                    runner
                        .native("knowledge reviews", &["wiki", "reviews", "--check"])
                        .await;
                }
                Operation::Process => {
                    runner.reconcile().await;
                    runner
                        .native("knowledge reviews", &["wiki", "reviews", "--check"])
                        .await;
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
        fn process_campaign_requires_a_verified_budget_checkpoint() {
            let mut result = JobResult::new(Operation::Process);
            result.ok = false;
            result.budget_exhausted = true;
            assert_eq!(result.campaign_stop(), CampaignStop::NoProgress);
            result.work_telemetry.retained_progress.action_receipts = 1;
            assert_eq!(result.campaign_stop(), CampaignStop::BatchLimit);
            result.halted = true;
            assert_eq!(result.campaign_stop(), CampaignStop::Failed);
            result.cancelled = true;
            assert_eq!(result.campaign_stop(), CampaignStop::Cancelled);
            result.cancelled = false;
            result.halted = false;
            result.budget_exhausted = false;
            assert_eq!(result.campaign_stop(), CampaignStop::Deferred);
            result.ok = true;
            assert_eq!(result.campaign_stop(), CampaignStop::Settled);
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
