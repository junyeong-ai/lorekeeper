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

    #[derive(Serialize)]
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
                agent_duration_ms: None,
                revalidation_record_duration_ms: None,
                verification_duration_ms: None,
                outcome: WorkOutcome::Rejected,
            }
        }
    }

    #[derive(Default, Serialize)]
    struct WorkTelemetry {
        work_steps: Vec<WorkStepMetrics>,
        work_steps_omitted: usize,
    }

    impl WorkTelemetry {
        const MAX_STEPS: usize = 1024;

        fn record(&mut self, step: WorkStepMetrics) {
            if self.work_steps.len() < Self::MAX_STEPS {
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
        Complete {
            root_node: String,
            preparation_duration_ms: u64,
        },
        Failed,
    }

    struct PreparedObservations {
        completed: usize,
        requests: std::collections::VecDeque<serde_json::Value>,
        preparation_duration_ms: Option<u64>,
    }

    struct ObservationTurn {
        work_id: String,
        node_id: String,
        completed: usize,
        request: serde_json::Value,
        metrics: WorkStepMetrics,
    }

    enum WorkStep {
        Pending,
        Staged,
        Deferred(String),
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
        Source {
            id: String,
        },
        Screening {
            task: lk_queue::QueueTask,
        },
        WholeInput {
            tasks: Vec<lk_queue::QueueTask>,
        },
    }

    struct QueuePlan {
        jobs: std::collections::BTreeMap<String, QueueDispatch>,
        deferred: std::collections::BTreeMap<String, Vec<String>>,
        keys: std::collections::BTreeSet<String>,
    }

    const MAX_PREPARED_QUEUE_TARGETS: usize = 64;

    struct QueueTurn {
        keys: std::collections::BTreeSet<String>,
        targets: Vec<String>,
    }

    #[derive(Default, Serialize)]
    struct QueuePlanningMetrics {
        attempts: usize,
        duration_ms: u64,
        synthesis_subjects: usize,
        audit_captures: usize,
        prepared_targets: usize,
        discovery_captures: usize,
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
        duration_ms: u64,
        semantic_duration_ms: u64,
        execution: Option<ExecutionInfo>,
        workflow_id: Option<lk_core::execution::WorkflowId>,
        semantic_calls: usize,
        #[serde(flatten)]
        work_telemetry: WorkTelemetry,
        queue_planning: QueuePlanningMetrics,
        audit: Option<Vec<lk_graph::audit::AuditCompletion>>,
        audit_progress: Option<AuditProgress>,
        audit_blocked: Vec<lk_graph::audit::AuditBlockedInput>,
        audit_blocked_count: usize,
        audit_deferred: Vec<DeferredAudit>,
        audit_deferred_count: usize,
        review_health: Option<ReviewHealth>,
        stages: Vec<StageResult>,
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
        audit_batch: Option<Vec<lk_graph::audit::AuditCandidate>>,
        queue_turn: Option<QueueTurn>,
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
            self.record(name, StageState::Failed, Some(error.to_string()))
        }

        fn configuration_is_current(&mut self) -> bool {
            if self.configuration_failed {
                return false;
            }
            let path = self.opts.config.as_ref().expect("resolved config");
            let current = path.canonicalize().and_then(|selected| {
                if selected != *path {
                    return Err(std::io::Error::other("selected configuration path changed"));
                }
                std::fs::read_to_string(path).map(|body| lk_core::concept::file_revision(&body))
            });
            let failure = match current {
                Ok(revision) if revision == self.config_revision => return true,
                Ok(_) => {
                    "selected configuration changed during the job; unfinished work is retained"
                        .into()
                }
                Err(error) => format!("selected configuration is unavailable: {error}"),
            };
            self.configuration_failed = true;
            self.failure("configuration", failure)
        }

        async fn child(
            &mut self,
            name: &str,
            mut command: tokio::process::Command,
            timeout: Option<Duration>,
        ) -> bool {
            if self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
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
            let completed = match self.supervisor.run(command, timeout, false).await {
                Ok(result) => self.record(
                    name,
                    if result.cancelled {
                        StageState::Cancelled
                    } else if result.success {
                        StageState::Completed
                    } else {
                        StageState::Failed
                    },
                    result.detail,
                ),
                Err(error) => self.failure(name, error),
            };
            self.configuration_is_current() && completed
        }

        async fn native(&mut self, name: &str, args: &[&str]) -> bool {
            if self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            match execution::lore_command(self.opts, args) {
                Ok(command) => self.child(name, command, None).await,
                Err(error) => self.failure(name, error),
            }
        }

        async fn acquire(&mut self, wait: bool) -> bool {
            if !self.configuration_is_current() {
                return false;
            }
            match self
                .supervisor
                .acquire(self.opts, &self.config, &self.config_revision, wait)
                .await
            {
                Ok(Some(lease)) => {
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
            if self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                return self.failure(skill, "semantic execution is disabled by llm.provider");
            }
            let budget = match self.config.execution.effective_work() {
                Ok(budget) => budget,
                Err(error) => return self.failure("semantic work budget", error),
            };
            if self.result.semantic_calls >= budget.max_calls {
                return self.failure("semantic call budget", "configured execution.work.max_calls exhausted; unfinished work is retained for the next run");
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
            let command = execution::agent_command(
                profile,
                skill,
                operation,
                self.installation,
                self.opts.config.as_ref().expect("resolved config"),
                input_directory,
            );
            match command {
                Ok(mut command) => {
                    if !self.reserve_queue_turn() {
                        return false;
                    }
                    self.result.semantic_calls += 1;
                    command.current_dir(&self.root);
                    let started_at = Instant::now();
                    let completed = self.child(skill, command, Some(timeout)).await;
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
                WorkRole::Evidence => execution::observation::build_invocation(profile, request),
                WorkRole::Screening => {
                    execution::observation::build_screening_invocation(profile, request)
                }
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
            if self.result.cancelled || !self.configuration_is_current() {
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
            if self.result.semantic_calls >= budget.max_calls {
                self.failure("semantic call budget", "configured execution.work.max_calls exhausted; unfinished work is retained for the next run");
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
            let timeout = self.selected.as_ref().expect("prepared agent").timeout();
            if !self.reserve_queue_turn() {
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
            metrics: &mut WorkStepMetrics,
        ) -> Option<crate::commands::work::state::WorkProgress> {
            let reply = self.request_observation(request, metrics).await?;
            let output =
                match execution::observation::decode(reply.engine, &reply.bytes, reply.limit) {
                    Ok(output) => output,
                    Err(error) => {
                        self.failure("bounded work receipt", error);
                        return None;
                    }
                };
            metrics.result_json_bytes = Some(
                serde_json::to_vec(&output)
                    .expect("decoded observation serializes")
                    .len(),
            );
            let started_at = Instant::now();
            let recorded = crate::commands::work::record_inspection(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
                node,
                output,
            );
            metrics.revalidation_record_duration_ms = Some(started_at.elapsed().as_millis() as u64);
            match recorded {
                Ok(progress) => {
                    self.record(
                        "bounded work receipt",
                        StageState::Completed,
                        Some(node.into()),
                    );
                    Some(progress)
                }
                Err(error) => {
                    self.failure("bounded work receipt", error);
                    None
                }
            }
        }

        fn call_budget_available(&mut self) -> bool {
            match self.config.execution.effective_work() {
                Ok(budget) if self.result.semantic_calls < budget.max_calls => true,
                Ok(_) => self.failure("semantic call budget", "configured execution.work.max_calls exhausted; unfinished work is retained for the next run"),
                Err(error) => self.failure("semantic work budget", error),
            }
        }

        fn reserve_queue_turn(&mut self) -> bool {
            let Some(turn) = self.queue_turn.take() else {
                return true;
            };
            match scheduler::reserve(
                &self.root,
                scheduler::Stage::Queue,
                &turn.keys,
                &turn.targets,
            ) {
                Ok(()) => true,
                Err(error) => self.failure("queue scheduling", error),
            }
        }

        async fn bounded_observation_wave(
            &mut self,
            selected: &str,
            jobs: &std::collections::BTreeMap<String, String>,
            keys: &std::collections::BTreeSet<String>,
            stage: scheduler::Stage,
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
            let mut prepared = std::collections::BTreeMap::<String, PreparedObservations>::new();
            let mut request_counts = std::collections::BTreeMap::<String, usize>::new();
            let mut planned_target = selected.to_owned();
            for _ in 0..limit {
                let Some(id) = jobs.get(&planned_target) else {
                    break;
                };
                *request_counts.entry(id.clone()).or_default() += 1;
                planned_target = keys
                    .range((
                        std::ops::Bound::Excluded(planned_target),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .or_else(|| keys.first())
                    .expect("selected keys")
                    .clone();
            }
            let mut turns = Vec::new();
            let mut reserved_targets = Vec::new();
            let mut target = selected.to_owned();
            while turns.len() < limit {
                let Some(id) = jobs.get(&target) else { break };
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    prepared.entry(id.clone())
                {
                    let started_at = Instant::now();
                    let captured = crate::commands::work::prepare_inspections(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        id,
                        request_counts[id],
                    );
                    let duration_ms = started_at.elapsed().as_millis() as u64;
                    let (progress, requests) = match captured {
                        Ok(captured) => captured,
                        Err(error) => return Some(self.failure("bounded work progress", error)),
                    };
                    if progress.complete {
                        break;
                    }
                    if requests.is_empty() {
                        return Some(self.failure(
                            "bounded work progress",
                            "incomplete work has no ready node",
                        ));
                    }
                    entry.insert(PreparedObservations {
                        completed: progress.completed,
                        requests: requests.into(),
                        preparation_duration_ms: Some(duration_ms),
                    });
                }
                let work = prepared.get_mut(id).expect("captured work");
                let Some(request) = work.requests.pop_front() else {
                    break;
                };
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
                metrics.preparation_duration_ms = work.preparation_duration_ms.take();
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
                target = keys
                    .range((
                        std::ops::Bound::Excluded(target),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .or_else(|| keys.first())
                    .expect("selected keys")
                    .clone();
            }
            if turns.is_empty() {
                return None;
            }
            if self.result.cancelled || !self.configuration_is_current() {
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
                    self.result.work_telemetry.record(turn.metrics);
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
            for turn in &turns {
                let (invocation, capture_limit) =
                    match self.observation_invocation(&turn.request, WorkRole::Evidence, budget) {
                        Ok(invocation) => invocation,
                        Err(error) => return Some(self.failure("bounded work receipt", error)),
                    };
                engines.push(invocation.engine);
                let (request, directory) = invocation.into_request(timeout, capture_limit);
                requests.push(request);
                directories.push(directory);
            }
            let reservations = if stage == scheduler::Stage::Queue {
                reserved_targets.as_slice()
            } else {
                &reserved_targets[1..]
            };
            if let Err(error) = scheduler::reserve(&self.root, stage, keys, reservations) {
                return Some(self.failure("bounded work scheduling", error));
            }
            self.result.semantic_calls += requests.len();
            let attempts = self.supervisor.run_requests(requests).await;
            drop(directories);
            let cancelled = attempts
                .iter()
                .any(|attempt| attempt.result.as_ref().is_ok_and(|child| child.cancelled));
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
            for ((mut turn, attempt), engine) in turns.into_iter().zip(attempts).zip(engines) {
                turn.metrics.agent_duration_ms = Some(attempt.duration_ms);
                self.result.semantic_duration_ms += attempt.duration_ms;
                if cancelled || !current {
                    turn.metrics.outcome = if cancelled {
                        WorkOutcome::Cancelled
                    } else {
                        WorkOutcome::Rejected
                    };
                    self.result.work_telemetry.record(turn.metrics);
                    continue;
                }
                let child = match attempt.result {
                    Ok(child) if child.success => child,
                    Ok(child) => {
                        accepted = false;
                        turn.metrics.outcome = WorkOutcome::Failed;
                        self.record("bounded work receipt", StageState::Failed, child.detail);
                        self.result.work_telemetry.record(turn.metrics);
                        continue;
                    }
                    Err(error) => {
                        accepted = false;
                        turn.metrics.outcome = WorkOutcome::Failed;
                        self.failure("bounded work receipt", error);
                        self.result.work_telemetry.record(turn.metrics);
                        continue;
                    }
                };
                let output = match execution::observation::decode(
                    engine,
                    &child.stdout,
                    budget.max_result_bytes,
                ) {
                    Ok(output) => output,
                    Err(error) => {
                        accepted = false;
                        self.failure("bounded work receipt", error);
                        self.result.work_telemetry.record(turn.metrics);
                        continue;
                    }
                };
                turn.metrics.result_json_bytes = Some(
                    serde_json::to_vec(&output)
                        .expect("observation serializes")
                        .len(),
                );
                if !self.configuration_is_current() {
                    accepted = false;
                    self.result.work_telemetry.record(turn.metrics);
                    continue;
                }
                let started_at = Instant::now();
                let recorded = crate::commands::work::record_inspection(
                    &self.root,
                    &self.config,
                    &self.config_revision,
                    &turn.work_id,
                    &turn.node_id,
                    output,
                );
                turn.metrics.revalidation_record_duration_ms =
                    Some(started_at.elapsed().as_millis() as u64);
                match recorded {
                    Ok(progress)
                        if progress.completed > turn.completed
                            && !progress.ready.contains(&turn.node_id) =>
                    {
                        turn.metrics.outcome = WorkOutcome::Recorded;
                        self.record(
                            "bounded work receipt",
                            StageState::Completed,
                            Some(turn.node_id),
                        );
                    }
                    Ok(_) => {
                        accepted = false;
                        self.failure("bounded work receipt", "worker exited without completing its selected node; unfinished work retained");
                    }
                    Err(error) => {
                        accepted = false;
                        self.failure("bounded work receipt", error);
                    }
                }
                self.result.work_telemetry.record(turn.metrics);
            }
            Some(accepted)
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
            );
            let preparation_duration_ms = started_at.elapsed().as_millis() as u64;
            metrics.preparation_duration_ms = Some(preparation_duration_ms);
            let (before, request) = match prepared {
                Ok(status) => status,
                Err(error) => {
                    self.result.work_telemetry.record(metrics);
                    self.failure("bounded work progress", error);
                    return ObservationStep::Failed;
                }
            };
            if !before.complete {
                let Some(node) = before.ready.first() else {
                    self.result.work_telemetry.record(metrics);
                    self.failure("bounded work progress", "incomplete work has no ready node");
                    return ObservationStep::Failed;
                };
                metrics.node_id = Some(node.clone());
                let Some(request) = request else {
                    self.result.work_telemetry.record(metrics);
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
                let result = match self.observe(id, node, request, &mut metrics).await {
                    Some(after)
                        if after.completed > before.completed && !after.ready.contains(node) =>
                    {
                        metrics.outcome = WorkOutcome::Recorded;
                        ObservationStep::Recorded
                    }
                    Some(_) => {
                        self.failure("bounded work receipt", "worker exited without completing its selected node; unfinished work retained");
                        ObservationStep::Failed
                    }
                    None => ObservationStep::Failed,
                };
                self.result.work_telemetry.record(metrics);
                return result;
            }
            ObservationStep::Complete {
                root_node: before.root_node,
                preparation_duration_ms,
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
            let (finished, action) = match response {
                execution::finalization::SectionResponse::Complete(body) => (
                    crate::commands::work::finish_section(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        id,
                        section,
                        body,
                    ),
                    None,
                ),
                execution::finalization::SectionResponse::NeedsAction(reason) => (
                    crate::commands::work::record_finalization_action(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        id,
                        section,
                        reason.clone(),
                    ),
                    Some(reason),
                ),
            };
            metrics.revalidation_record_duration_ms = Some(started_at.elapsed().as_millis() as u64);
            match finished {
                Ok(()) => match action {
                    Some(reason) => {
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
                ObservationStep::Failed => return WorkStep::Failed,
                ObservationStep::Complete {
                    root_node,
                    preparation_duration_ms,
                } => {
                    metrics.node_id = Some(root_node);
                    metrics.preparation_duration_ms = Some(preparation_duration_ms);
                }
            }
            let started_at = Instant::now();
            let finished = crate::commands::work::is_finished(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
            );
            metrics.verification_duration_ms = Some(started_at.elapsed().as_millis() as u64);
            match finished {
                Ok(true) => {
                    let started_at = Instant::now();
                    let restaged = crate::commands::work::restage_finished(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        id,
                    );
                    metrics.revalidation_record_duration_ms =
                        Some(started_at.elapsed().as_millis() as u64);
                    if let Err(error) = restaged {
                        self.result.work_telemetry.record(metrics);
                        self.failure("bounded work completion", error);
                        return WorkStep::Failed;
                    }
                    metrics.outcome = WorkOutcome::Restaged;
                    self.result.work_telemetry.record(metrics);
                    self.record(
                        "bounded work completion",
                        StageState::Skipped,
                        Some("verified sealed result already staged".into()),
                    );
                    return WorkStep::Staged;
                }
                Ok(false) => {}
                Err(error) => {
                    self.result.work_telemetry.record(metrics);
                    self.failure("bounded work completion", error);
                    return WorkStep::Failed;
                }
            }
            let started_at = Instant::now();
            let section = crate::commands::work::prepare_section_finalization(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
            );
            *metrics
                .preparation_duration_ms
                .as_mut()
                .expect("measured preparation") += started_at.elapsed().as_millis() as u64;
            match section {
                Ok(crate::commands::work::SectionPreparation::Interactive) => {}
                Ok(crate::commands::work::SectionPreparation::NeedsAction(reason)) => {
                    self.record(
                        "bounded section action",
                        StageState::Skipped,
                        Some(reason.clone()),
                    );
                    self.result.work_telemetry.record(metrics);
                    return WorkStep::Deferred(reason);
                }
                Ok(crate::commands::work::SectionPreparation::Ready(section)) => {
                    let result = self.finalize_section(id, section, &mut metrics).await;
                    self.result.work_telemetry.record(metrics);
                    return result;
                }
                Err(error) => {
                    self.result.work_telemetry.record(metrics);
                    self.failure("bounded section finalization", error);
                    return WorkStep::Failed;
                }
            }
            let operation = format!(
                "bounded finish {id}. Start at the bounded-work reference's Finalize section. Read lore work result once for this exact work and retain its response; use lore work status for the distinct context_revision publication guard. Read only the matching generation rules and result-envelope sections named there. Produce the typed final result from the completed guarded observations; do not restart ordinary intake, discovery, screening or full-evidence reading. Exact name/alias ownership checks and targeted revision-pinned reads remain necessary when decision-relevant information is missing; expanded discovery or changed inputs require replanning. Submit with lore work finish. Do not read the whole corpus or drain other tasks. Leave the staged queue result or audit assessment for native controller publication. The controller owns global checks. Preserve source_questions as attributed uncertainty in the final prose; do not invent answers. Material conflicts need a scoped open review and follow-up audit disposition. Processing blockers in unresolved cannot be silently removed or marked reviewed."
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
                self.result.work_telemetry.record(metrics);
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
            self.result.work_telemetry.record(metrics);
            result
        }

        async fn screening_step(&mut self, task: &lk_queue::QueueTask) -> Option<bool> {
            let mut metrics = WorkStepMetrics::new(WorkRole::Screening);
            let result = self.screen_candidates(task, &mut metrics).await;
            self.result.work_telemetry.record(metrics);
            result
        }

        async fn screen_candidates(
            &mut self,
            task: &lk_queue::QueueTask,
            metrics: &mut WorkStepMetrics,
        ) -> Option<bool> {
            let started_at = Instant::now();
            let prepared = crate::commands::work::prepare_screening(
                &self.root,
                &self.config,
                &self.config_revision,
                &task.task_id,
            );
            metrics.preparation_duration_ms = Some(started_at.elapsed().as_millis() as u64);
            let before = match prepared {
                Ok(before) => before,
                Err(error) => {
                    self.failure("candidate screening", error);
                    return None;
                }
            };
            metrics.selection_id = Some(before.selection_id.clone());
            if before.complete {
                metrics.outcome = WorkOutcome::AlreadyComplete;
                return Some(true);
            }
            let Some(request) = before.request else {
                self.failure(
                    "candidate screening",
                    format!(
                        "{} deferred and {} unknown candidate decisions remain; work retained",
                        before.deferred, before.unknown
                    ),
                );
                return None;
            };
            let selected_input = request["input_revision"].clone();
            let selected_batch = request["batch_id"].clone();
            metrics.batch_id = selected_batch.as_str().map(str::to_owned);
            let reply = self.request_observation(request, metrics).await?;
            let output = match execution::observation::decode_response(
                reply.engine,
                &reply.bytes,
                reply.limit,
            ) {
                Ok(output) => output,
                Err(error) => {
                    self.failure("candidate screening", error);
                    return None;
                }
            };
            metrics.result_json_bytes = Some(
                serde_json::to_vec(&output)
                    .expect("decoded screening serializes")
                    .len(),
            );
            if output["selection_id"] != before.selection_id
                || output["batch_id"] != selected_batch
                || output["input_revision"] != selected_input
            {
                self.failure("candidate screening", "worker did not answer the exact selected screening packet; unfinished work retained");
                return None;
            }
            let started_at = Instant::now();
            let recorded = crate::commands::work::record_screening(
                &self.root,
                &self.config,
                &self.config_revision,
                &task.task_id,
                output,
                None,
            );
            metrics.revalidation_record_duration_ms = Some(started_at.elapsed().as_millis() as u64);
            let completed = match recorded {
                Ok(completed) if completed > before.completed_batches => completed,
                Ok(_) => {
                    self.failure(
                        "candidate screening",
                        "worker did not advance the selected screening batch; work retained",
                    );
                    return None;
                }
                Err(error) => {
                    self.failure("candidate screening", error);
                    return None;
                }
            };
            metrics.outcome = WorkOutcome::Recorded;
            self.record("candidate screening", StageState::Completed,
                Some(format!("{}/{} screening batches recorded; card decisions are not full-body assessments", completed, before.batches)));
            Some(completed == before.batches)
        }

        fn queue_dispatches(
            &mut self,
            blocked: &std::collections::BTreeMap<String, String>,
        ) -> miette::Result<QueuePlan> {
            fn flush(
                batch: &mut Vec<lk_queue::QueueTask>,
                jobs: &mut std::collections::BTreeMap<String, QueueDispatch>,
            ) {
                if let Some(first) = batch.first() {
                    jobs.insert(
                        first.target.vault_path.clone(),
                        QueueDispatch::WholeInput {
                            tasks: std::mem::take(batch),
                        },
                    );
                }
            }
            let session = lk_vault::MutationSession::acquire(&self.root)
                .map_err(|error| miette::miette!("{error}"))?;
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
            for task in owners.into_values() {
                let target = task.target.vault_path.clone();
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
            if !targets.is_empty() && self.result.semantic_calls >= budget.max_calls {
                return Ok(QueuePlan {
                    jobs,
                    deferred,
                    keys,
                });
            }
            let order = scheduler::order(session.root(), scheduler::Stage::Queue, &keys)?;
            for window in order.chunks(MAX_PREPARED_QUEUE_TARGETS) {
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
                )?;
                self.result.queue_planning.discovery_captures += captures;
                let mut prepared = materialize
                    .into_iter()
                    .zip(prepared)
                    .map(|(task, work)| (task.target.vault_path.clone(), (task, work)))
                    .collect::<std::collections::BTreeMap<_, _>>();
                let mut batch = Vec::new();
                let mut batch_bytes = 0usize;
                for target in window {
                    let Some((task, work)) = prepared.remove(target) else {
                        flush(&mut batch, &mut jobs);
                        batch_bytes = 0;
                        continue;
                    };
                    match work {
                        crate::commands::work::TaskWork::WholeInput { bytes } => {
                            if bytes > budget.max_input_bytes.saturating_sub(batch_bytes) {
                                flush(&mut batch, &mut jobs);
                                batch_bytes = 0;
                            }
                            batch_bytes += bytes;
                            batch.push(task);
                        }
                        crate::commands::work::TaskWork::Partitioned { id } => {
                            flush(&mut batch, &mut jobs);
                            batch_bytes = 0;
                            if let Some(reason) = blocked.get(&id) {
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
                            batch_bytes = 0;
                            jobs.insert(target.clone(), QueueDispatch::Source { id });
                        }
                        crate::commands::work::TaskWork::Screening => {
                            flush(&mut batch, &mut jobs);
                            batch_bytes = 0;
                            jobs.insert(target.clone(), QueueDispatch::Screening { task });
                        }
                    }
                }
                flush(&mut batch, &mut jobs);
                if !jobs.is_empty() {
                    break;
                }
            }
            Ok(QueuePlan {
                jobs,
                deferred,
                keys,
            })
        }

        fn expanded_extraction_handoff(
            &self,
            tasks: &[lk_queue::QueueTask],
        ) -> miette::Result<bool> {
            let mut unfinished = false;
            for task in tasks {
                if crate::commands::queue::tasks_completed(&self.root, std::slice::from_ref(task))?
                {
                    continue;
                }
                unfinished = true;
                if task.kind != lk_queue::TaskKind::ExtractConcepts {
                    return Ok(false);
                }
                let current = crate::commands::queue::current_task(&self.root, &task.task_id)?;
                if current.kind != task.kind
                    || current.cache_hash != task.cache_hash
                    || current.target != task.target
                    || current.input != task.input
                    || current.created_at != task.created_at
                    || !crate::commands::work::has_current_discovery(&self.root, &current)?
                    || !matches!(
                        crate::commands::work::prepare_task(
                            &self.root,
                            &self.config,
                            &self.config_revision,
                            &task.task_id,
                        )?,
                        crate::commands::work::TaskWork::Partitioned { .. }
                            | crate::commands::work::TaskWork::Source { .. }
                            | crate::commands::work::TaskWork::Screening
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
                self.queue_turn = None;
                let started_at = Instant::now();
                self.result.queue_planning.attempts += 1;
                let planned = self.queue_dispatches(&blocked);
                self.result.queue_planning.duration_ms += started_at.elapsed().as_millis() as u64;
                let plan = match planned {
                    Ok(plan) => plan,
                    Err(error) => return self.failure("queue planning", error),
                };
                let jobs = plan.jobs;
                if jobs.is_empty() {
                    if !plan.keys.is_empty() && !self.call_budget_available() {
                        return false;
                    }
                    if let Some((target, reasons)) = plan.deferred.first_key_value() {
                        return self.failure(
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
                let bounded_jobs =
                    jobs.iter()
                        .filter_map(|(target, job)| match job {
                            QueueDispatch::Partitioned { id, .. }
                            | QueueDispatch::Source { id } => Some((target.clone(), id.clone())),
                            _ => None,
                        })
                        .collect();
                loop {
                    if !self.call_budget_available() {
                        return false;
                    }
                    let keys = &plan.keys;
                    let selected = match scheduler::order(&self.root, scheduler::Stage::Queue, keys)
                    {
                        Ok(order) if !order.is_empty() => order[0].clone(),
                        Ok(_) => return true,
                        Err(error) => return self.failure("queue scheduling", error),
                    };
                    if !jobs.contains_key(&selected) {
                        continue 'planning;
                    }
                    if let Some(completed) = self
                        .bounded_observation_wave(
                            &selected,
                            &bounded_jobs,
                            keys,
                            scheduler::Stage::Queue,
                        )
                        .await
                    {
                        if !completed {
                            return false;
                        }
                        continue;
                    }
                    self.queue_turn = Some(QueueTurn {
                        keys: keys.clone(),
                        targets: match &jobs[&selected] {
                            QueueDispatch::WholeInput { tasks } => tasks
                                .iter()
                                .map(|task| task.target.vault_path.clone())
                                .collect(),
                            _ => vec![selected.clone()],
                        },
                    });
                    match &jobs[&selected] {
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
                                return false;
                            }
                            match crate::commands::work::has_current_discovery(&self.root, task) {
                                Ok(true) => continue 'planning,
                                Ok(false) => return self.failure("extraction discovery receipt", "selected extraction has no current discovery recipe; unfinished work retained"),
                                Err(error) => return self.failure("extraction discovery receipt", error),
                            }
                        }
                        QueueDispatch::Partitioned { id, .. } => {
                            match self.bounded_step(id, "lore-process").await {
                                WorkStep::Pending => continue,
                                WorkStep::Deferred(reason) => {
                                    blocked.insert(id.clone(), reason);
                                    continue 'planning;
                                }
                                WorkStep::Failed => return false,
                                WorkStep::Staged => {}
                            }
                        }
                        QueueDispatch::Source { id, .. } => {
                            match self.bounded_observation_step(id).await {
                                ObservationStep::Recorded => continue,
                                ObservationStep::Complete {
                                    root_node,
                                    preparation_duration_ms,
                                } => {
                                    let mut metrics = WorkStepMetrics::new(WorkRole::Evidence);
                                    metrics.work_id = Some(id.clone());
                                    metrics.node_id = Some(root_node);
                                    metrics.preparation_duration_ms = Some(preparation_duration_ms);
                                    metrics.outcome = WorkOutcome::AlreadyComplete;
                                    self.result.work_telemetry.record(metrics);
                                    continue 'planning;
                                }
                                ObservationStep::Failed => return false,
                            }
                        }
                        QueueDispatch::Screening { task } => {
                            match self.screening_step(task).await {
                                Some(true) => continue 'planning,
                                Some(false) => continue,
                                None => return false,
                            }
                        }
                        QueueDispatch::WholeInput { tasks } => {
                            let ids: Vec<_> = tasks.iter().map(|task| &task.task_id).collect();
                            let directory = match tempfile::Builder::new()
                                .prefix("lorekeeper-selection-")
                                .tempdir()
                            {
                                Ok(directory) => directory,
                                Err(error) => return self.failure("queue selection", error),
                            };
                            let path = directory.path().join("tasks.json");
                            if let Err(error) = std::fs::write(
                                &path,
                                serde_json::to_vec(&ids).expect("task ids serialize"),
                            ) {
                                return self.failure("queue selection", error);
                            }
                            let selection = match serde_json::to_string(&path) {
                                Ok(selection) => selection,
                                Err(error) => return self.failure("queue selection", error),
                            };
                            let context = if matches!(self.result.operation, Operation::Knowledge) {
                                "Process these tasks as part of this bounded knowledge maintenance job. Read the selected work's review dependencies. Do not enumerate unrelated reviews or expand this batch."
                            } else {
                                ""
                            };
                            let operation = format!(
                                "selected. {context} The native controller selected one task per distinct target within the configured evidence input budget. Read the JSON array of exact task IDs from the selection manifest and process only those current tasks, then return. Do not drain the rest of the queue or run global finalization. Follow the ordinary typed staging/publication protocol for these tasks. For extraction, record exact discovery recipes and inspect lore work context before loading candidate bodies. Follow its needs-discovery, needs-source-observations, needs-screening and ready states. Complete ordinary small-source screening in this selected session when practical. If a phase requires source observations, bounded candidate screening or partitioned admitted evidence, execute its returned plan_command and stage no extraction result; finish other selected tasks and return so the controller can resume that exact source/screening/evidence phase. Draft only from ready context, carry its typed selection when present, and fully read every selected reused owner. Unknown or deferred candidates remain unfinished; not-selected is a candidate-card judgment rather than a whole-body finding. Selected queue task manifest: {selection}"
                            );
                            if !self
                                .consumer("lore-process", &operation, Some(directory.path()))
                                .await
                            {
                                return false;
                            }
                        }
                    }
                    self.queue_turn = None;
                    if !self
                        .native("selected queue apply", &["queue", "apply"])
                        .await
                    {
                        return false;
                    }
                    let tasks = match &jobs[&selected] {
                        QueueDispatch::WholeInput { tasks } => tasks.as_slice(),
                        QueueDispatch::Partitioned { task, .. } => std::slice::from_ref(task),
                        QueueDispatch::Discovery { .. } => {
                            unreachable!("discovery restarts planning")
                        }
                        QueueDispatch::Source { .. } | QueueDispatch::Screening { .. } => {
                            unreachable!("preparation restarts planning without publication")
                        }
                    };
                    match crate::commands::queue::tasks_completed(&self.root, tasks) {
                        Ok(true) => {}
                        Ok(false) => {
                            if matches!(jobs[&selected], QueueDispatch::WholeInput { .. }) {
                                match self.expanded_extraction_handoff(tasks) {
                                    Ok(true) => {
                                        self.record("expanded extraction work", StageState::Completed, Some("discovery expanded unfinished extraction beyond the whole-input budget; replanning bounded inspections".into()));
                                        continue 'planning;
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        return self.failure("selected queue receipt", error);
                                    }
                                }
                            }
                            return self.failure("selected queue receipt", "selected tasks are not completed against their retained inputs; unfinished work retained");
                        }
                        Err(error) => return self.failure("selected queue receipt", error),
                    }
                    if tasks
                        .iter()
                        .any(|task| task.kind != lk_queue::TaskKind::SynthesizeConcept)
                        && !self
                            .native(
                                "queue publication reconciliation",
                                &["graph", "backlinks-sync"],
                            )
                            .await
                    {
                        return false;
                    }
                    break;
                }
            }
        }

        async fn drain(&mut self) -> bool {
            if self.result.cancelled {
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
                    self.drain_blocked = !completed;
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
            if paths.is_empty() {
                let plan = lk_graph::audit::plan_available_partitioned(
                    &self.root,
                    &self.config.vault.dirs,
                    &pages,
                    self.config.graph.audit.batch_size,
                    self.config.graph.audit.max_input_bytes,
                    &mut |candidate| readiness.check(candidate),
                )
                .map_err(|error| miette::miette!("{error}"))?;
                self.result.audit_deferred = readiness.deferred().to_vec();
                self.result.audit_deferred_count = readiness.deferred_count();
                return Ok(plan);
            }
            let plan = lk_graph::audit::plan_selected_partitioned(
                &self.root,
                &self.config.vault.dirs,
                &pages,
                self.config.graph.audit.batch_size,
                paths,
                self.config.graph.audit.max_input_bytes,
            )
            .map_err(|e| miette::miette!("{e}"))?;
            for candidate in &plan.candidates {
                readiness.require_ready(candidate)?;
            }
            Ok(plan)
        }

        fn admit_audit(&mut self) -> miette::Result<lk_graph::audit::AuditPlan> {
            let session = lk_vault::MutationSession::acquire(&self.root)
                .map_err(|error| miette::miette!("{error}"))?;
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
            if !self.result.ok || self.result.cancelled {
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
                self.native(name, &args).await;
            }
        }

        async fn audit(&mut self) {
            if !self.inspect_review_health() {
                return;
            }
            let mut plan = match self.admit_audit() {
                Ok(plan) => plan,
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
                    self.failure("audit dependencies", "pending subjects have unfinished semantic inputs; retained for later assessment");
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
            let selection = serde_json::to_string(&paths).expect("string paths serialize");
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
            if bounded {
                let mut jobs = std::collections::BTreeMap::new();
                for candidate in &plan.candidates {
                    let path = &candidate.path;
                    let work = match crate::commands::work::prepare_audit(
                        &self.root,
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
                    if !self.call_budget_available() {
                        return;
                    }
                    let keys = jobs.keys().cloned().collect();
                    let selected =
                        match scheduler::select(&self.root, scheduler::Stage::Audit, &keys) {
                            Ok(Some(selected)) => selected,
                            Ok(None) => break,
                            Err(error) => {
                                self.failure("audit scheduling", error);
                                return;
                            }
                        };
                    let work = &jobs[&selected];
                    if let Some(completed) = self
                        .bounded_observation_wave(&selected, &jobs, &keys, scheduler::Stage::Audit)
                        .await
                    {
                        if !completed {
                            return;
                        }
                        continue;
                    }
                    match self.bounded_step(work, "lore-wiki").await {
                        WorkStep::Pending => continue,
                        WorkStep::Deferred(reason) => {
                            self.failure("bounded audit action", reason);
                            return;
                        }
                        WorkStep::Failed => return,
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
                    jobs.remove(&selected);
                }
            } else {
                self.consumer("lore-wiki", &format!("maintain. The controller selected this exact batch: {selection}. Start with audit-plan --path for these addresses; do not select another batch. Use bounded work for any subject whose work envelope requires partitioning."), None).await;
            }
            self.audit_batch = Some(plan.candidates);
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
        if batches == 0 || batches != 1 && !matches!(operation, Operation::Knowledge) {
            return Err(miette::miette!(
                "multiple batches are only supported for knowledge jobs"
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
        let mut stop_reason = "batch-limit";
        for _ in 0..batches {
            let result = execute(
                &options,
                installation,
                config.clone(),
                config_revision.clone(),
                operation,
                wait,
                &mut supervisor,
            )
            .await;
            stop_reason = if result.cancelled {
                "cancelled"
            } else if result.audit.as_ref().is_some_and(Vec::is_empty)
                && result.audit_blocked_count > 0
            {
                "blocked"
            } else if !result.ok {
                "failed"
            } else if result.audit.as_ref().is_some_and(Vec::is_empty)
                && result
                    .audit_progress
                    .as_ref()
                    .and_then(|progress| progress.after.as_ref())
                    .is_some_and(|coverage| coverage.unreviewed == 0 && coverage.changed == 0)
            {
                "settled"
            } else {
                "batch-limit"
            };
            outcomes.push(result);
            if stop_reason != "batch-limit" {
                break;
            }
        }
        let ok = outcomes.iter().all(|result| result.ok);
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
        config: Config,
        config_revision: String,
        operation: Operation,
        wait: bool,
        supervisor: &mut execution::Supervisor,
    ) -> JobResult {
        let board = config
            .personal
            .as_ref()
            .and_then(|personal| personal.tasks.as_ref())
            .is_some();
        let mut runner = Runner {
            installation,
            root: config.vault.root_path(),
            config,
            config_revision,
            configuration_failed: false,
            opts: options,
            supervisor,
            lease: None,
            selected: None,
            project_protection: None,
            drain_blocked: false,
            audit_batch: None,
            queue_turn: None,
            started_at: Instant::now(),
            result: JobResult {
                operation,
                ok: true,
                cancelled: false,
                duration_ms: 0,
                semantic_duration_ms: 0,
                execution: None,
                workflow_id: None,
                semantic_calls: 0,
                work_telemetry: WorkTelemetry::default(),
                queue_planning: QueuePlanningMetrics::default(),
                audit: None,
                audit_progress: None,
                audit_blocked: Vec::new(),
                audit_blocked_count: 0,
                audit_deferred: Vec::new(),
                audit_deferred_count: 0,
                review_health: None,
                stages: Vec::new(),
            },
        };
        if !runner.configuration_is_current() {
            return runner.finish();
        }
        if matches!(operation, Operation::Daily) {
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
                        &["ingest", "--recover", "--wait-for-vault"],
                    )
                    .await;
            }
            runner
                .native("ingest", &["ingest", "--wait-for-vault"])
                .await;
        }
        if !runner.result.cancelled && runner.acquire(wait).await {
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
