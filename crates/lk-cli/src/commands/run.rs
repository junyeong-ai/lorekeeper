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
    operation: Operation,
    profile: Option<String>,
    batches: usize,
    wait: bool,
    json: bool,
) -> miette::Result<()> {
    #[cfg(unix)]
    {
        native::run(opts, operation, profile, batches, wait, json).await
    }
    #[cfg(not(unix))]
    {
        let _ = (opts, operation, profile, batches, wait, json);
        Err(miette::miette!(
            "agent jobs require Unix workflow leases; Windows is unsupported"
        ))
    }
}

#[cfg(unix)]
mod native {
    use super::scheduler;
    use super::*;
    use crate::commands::{
        CONFIG_REVISION_ENV, EXECUTION_PROFILE_ENV, GlobalOptions, execution, workflow,
    };
    use lk_core::config::Config;
    use lk_core::execution::ExecutionProfile;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    enum WorkStep {
        Pending,
        Staged,
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
        WholeInput {
            tasks: Vec<lk_queue::QueueTask>,
        },
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
    struct DeferredAudit {
        path: String,
        reasons: Vec<String>,
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
        audit: Option<Vec<lk_graph::audit::AuditCompletion>>,
        audit_progress: Option<AuditProgress>,
        audit_blocked: Vec<lk_graph::audit::AuditBlockedInput>,
        audit_blocked_count: usize,
        audit_deferred: Vec<DeferredAudit>,
        review_health: Option<ReviewHealth>,
        stages: Vec<StageResult>,
    }

    struct Runner<'a> {
        opts: &'a GlobalOptions,
        config: Config,
        config_revision: String,
        configuration_failed: bool,
        root: PathBuf,
        supervisor: &'a mut execution::Supervisor,
        lease: Option<workflow::WorkflowLease>,
        selected: Option<ExecutionProfile>,
        drain_blocked: bool,
        audit_batch: Option<Vec<lk_graph::audit::AuditCandidate>>,
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
            let command = lk_dist::Installation::detect()
                .map_err(|e| miette::miette!("{e}"))
                .and_then(|installation| {
                    execution::agent_command(
                        profile,
                        skill,
                        operation,
                        &installation,
                        self.opts.config.as_ref().expect("resolved config"),
                        input_directory,
                    )
                });
            match command {
                Ok(mut command) => {
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

        async fn observe(&mut self, id: &str, node: &str) -> bool {
            if self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                return self.failure(
                    "bounded work receipt",
                    "semantic execution is disabled by llm.provider",
                );
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
                    self.record(
                        "bounded work receipt",
                        StageState::Cancelled,
                        Some(error.to_string()),
                    )
                } else {
                    self.failure("bounded work receipt", error)
                };
            }
            let request = match crate::commands::work::inspection_input(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
                node,
            ) {
                Ok(input) => input,
                Err(error) => return self.failure("bounded work receipt", error),
            };
            let profile = self.selected.as_ref().expect("prepared agent");
            let invocation = match execution::observation::build_invocation(profile, &request) {
                Ok(invocation) => invocation,
                Err(error) => return self.failure("bounded work receipt", error),
            };
            let timeout = profile.timeout();
            let mut command = invocation.command;
            if let Some(lease) = &self.lease {
                lease.configure(command.as_std_mut());
            }
            command.env(CONFIG_REVISION_ENV, &self.config_revision).env(
                "LORE_CONFIG",
                self.opts.config.as_ref().expect("resolved config"),
            );
            if let Some(name) = self.config.execution.selected_profile() {
                command.env(EXECUTION_PROFILE_ENV, name);
            }
            let Some(limit) = budget
                .max_result_bytes
                .checked_mul(3)
                .and_then(|limit| limit.checked_add(16384))
            else {
                return self.failure(
                    "bounded work receipt",
                    "host output envelope limit overflow",
                );
            };
            self.result.semantic_calls += 1;
            let started_at = Instant::now();
            let outcome = self
                .supervisor
                .run_request(
                    command,
                    Some(timeout),
                    Some(limit),
                    Some(&invocation.prompt),
                )
                .await;
            self.result.semantic_duration_ms += started_at.elapsed().as_millis() as u64;
            let child = match outcome {
                Ok(child) => child,
                Err(error) => return self.failure("bounded work receipt", error),
            };
            if !child.success {
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
                return false;
            }
            if !self.configuration_is_current() {
                return false;
            }
            let output = match execution::observation::decode(
                invocation.engine,
                &child.stdout,
                budget.max_result_bytes,
            ) {
                Ok(output) => output,
                Err(error) => return self.failure("bounded work receipt", error),
            };
            match crate::commands::work::record_inspection(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
                node,
                output,
            ) {
                Ok(()) => self.record(
                    "bounded work receipt",
                    StageState::Completed,
                    Some(node.into()),
                ),
                Err(error) => self.failure("bounded work receipt", error),
            }
        }

        fn call_budget_available(&mut self) -> bool {
            match self.config.execution.effective_work() {
                Ok(budget) if self.result.semantic_calls < budget.max_calls => true,
                Ok(_) => self.failure("semantic call budget", "configured execution.work.max_calls exhausted; unfinished work is retained for the next run"),
                Err(error) => self.failure("semantic work budget", error),
            }
        }

        async fn bounded_step(&mut self, id: &str, skill: &str) -> WorkStep {
            let before = match crate::commands::work::current_progress(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
            ) {
                Ok(status) => status,
                Err(error) => {
                    self.failure("bounded work progress", error);
                    return WorkStep::Failed;
                }
            };
            if !before.complete {
                let Some(node) = before.ready.first() else {
                    self.failure("bounded work progress", "incomplete work has no ready node");
                    return WorkStep::Failed;
                };
                if !self.observe(id, node).await {
                    return WorkStep::Failed;
                }
                return match crate::commands::work::current_progress(
                    &self.root,
                    &self.config,
                    &self.config_revision,
                    id,
                ) {
                    Ok(after)
                        if after.completed > before.completed && !after.ready.contains(node) =>
                    {
                        WorkStep::Pending
                    }
                    Ok(_) => {
                        self.failure("bounded work receipt", "worker exited without completing its selected node; unfinished work retained");
                        WorkStep::Failed
                    }
                    Err(error) => {
                        self.failure("bounded work receipt", error);
                        WorkStep::Failed
                    }
                };
            }
            match crate::commands::work::is_finished(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
            ) {
                Ok(true) => {
                    if let Err(error) = crate::commands::work::restage_finished(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        id,
                    ) {
                        self.failure("bounded work completion", error);
                        return WorkStep::Failed;
                    }
                    self.record(
                        "bounded work completion",
                        StageState::Skipped,
                        Some("verified sealed result already staged".into()),
                    );
                    return WorkStep::Staged;
                }
                Ok(false) => {}
                Err(error) => {
                    self.failure("bounded work completion", error);
                    return WorkStep::Failed;
                }
            }
            let operation = format!(
                "bounded finish {id}. Read the bounded-work reference and lore work result for this exact work. Produce the normal typed final result using its completed root observation and selected generation specification. Submit with lore work finish. Do not read the whole corpus or drain other tasks. Leave the staged queue result or audit assessment for native controller publication. The controller owns global checks. Preserve source_questions as attributed uncertainty in the final prose; do not invent answers. Material conflicts need a scoped open review and follow-up audit disposition. Processing blockers in unresolved cannot be silently removed or marked reviewed."
            );
            if !self.consumer(skill, &operation, None).await {
                return WorkStep::Failed;
            }
            match crate::commands::work::verify_finished(
                &self.root,
                &self.config,
                &self.config_revision,
                id,
            ) {
                Ok(()) => {
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
            }
        }

        fn queue_dispatches(
            &self,
        ) -> miette::Result<std::collections::BTreeMap<String, QueueDispatch>> {
            fn flush(
                batch: &mut Vec<lk_queue::QueueTask>,
                jobs: &mut std::collections::BTreeMap<String, QueueDispatch>,
            ) {
                if let Some(last) = batch.last() {
                    jobs.insert(
                        last.target.vault_path.clone(),
                        QueueDispatch::WholeInput {
                            tasks: std::mem::take(batch),
                        },
                    );
                }
            }
            let mut tasks = crate::commands::queue::current_tasks(&self.root)?;
            tasks.sort_by(|left, right| {
                (left.created_at, &left.task_id).cmp(&(right.created_at, &right.task_id))
            });
            let mut targets = std::collections::BTreeMap::new();
            for task in tasks {
                targets
                    .entry(task.target.vault_path.clone())
                    .or_insert(task);
            }
            let limit = self
                .config
                .execution
                .effective_work()
                .map_err(|error| miette::miette!("{error}"))?
                .max_input_bytes;
            let mut jobs = std::collections::BTreeMap::new();
            let mut batch = Vec::new();
            let mut batch_bytes = 0usize;
            for (target, task) in targets {
                if crate::commands::work::needs_discovery(
                    &self.root,
                    &self.config,
                    &self.config_revision,
                    &task,
                )? {
                    flush(&mut batch, &mut jobs);
                    batch_bytes = 0;
                    jobs.insert(target, QueueDispatch::Discovery { task });
                    continue;
                }
                match crate::commands::work::prepare_task(
                    &self.root,
                    &self.config,
                    &self.config_revision,
                    &task.task_id,
                )? {
                    crate::commands::work::TaskWork::WholeInput { bytes } => {
                        if bytes > limit.saturating_sub(batch_bytes) {
                            flush(&mut batch, &mut jobs);
                            batch_bytes = 0;
                        }
                        batch_bytes += bytes;
                        batch.push(task);
                    }
                    crate::commands::work::TaskWork::Partitioned { id } => {
                        flush(&mut batch, &mut jobs);
                        batch_bytes = 0;
                        jobs.insert(target, QueueDispatch::Partitioned { id, task });
                    }
                }
            }
            flush(&mut batch, &mut jobs);
            Ok(jobs)
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
                    )
                {
                    return Ok(false);
                }
            }
            Ok(unfinished)
        }

        async fn dispatch_queue(&mut self) -> bool {
            'planning: loop {
                let jobs = match self.queue_dispatches() {
                    Ok(jobs) => jobs,
                    Err(error) => return self.failure("queue planning", error),
                };
                if jobs.is_empty() {
                    return true;
                }
                loop {
                    if !self.call_budget_available() {
                        return false;
                    }
                    let keys = jobs.keys().cloned().collect();
                    let selected =
                        match scheduler::select(&self.root, scheduler::Stage::Queue, &keys) {
                            Ok(Some(selected)) => selected,
                            Ok(None) => return true,
                            Err(error) => return self.failure("queue scheduling", error),
                        };
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
                                WorkStep::Failed => return false,
                                WorkStep::Staged => {}
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
                                "selected. {context} The native controller selected one task per distinct target within the configured evidence input budget. Read the JSON array of exact task IDs from the selection manifest and process only those current tasks, then return. Do not drain the rest of the queue or run global finalization. Follow the ordinary typed staging/publication protocol for these tasks. For extraction, record exact discovery recipes and inspect lore work context before loading the expanded candidate corpus. If requires_bounded_work is true, execute its plan_command and stage no result for that task; finish other selected tasks and return so the controller can schedule its bounded inspections. Selected queue task manifest: {selection}"
                            );
                            if !self
                                .consumer("lore-process", &operation, Some(directory.path()))
                                .await
                            {
                                return false;
                            }
                        }
                    }
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
                    break;
                }
            }
        }

        async fn drain(&mut self) -> bool {
            if self.result.cancelled {
                return false;
            }
            match crate::commands::queue::check::inspect(&self.root) {
                Ok(state) if state.current == 0 => self.record(
                    "queue drain",
                    StageState::Skipped,
                    Some("no current tasks".into()),
                ),
                Ok(_) if self.config.llm.provider == lk_core::config::LlmProvider::Noop => self
                    .failure(
                        "queue drain",
                        "semantic processing is disabled by llm.provider",
                    ),
                Ok(_) if self.drain_blocked => {
                    self.record(
                        "queue drain",
                        StageState::Skipped,
                        Some("previous consumer failed; pending work retained".into()),
                    );
                    false
                }
                Ok(_) => {
                    let completed = self.dispatch_queue().await;
                    self.drain_blocked = !completed;
                    completed
                }
                Err(error) => self.failure("queue inspection", error),
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
            let mut readiness = super::super::queue::readiness::AuditReadiness::load(&self.root)?;
            if paths.is_empty() {
                let mut deferred = Vec::new();
                let plan = lk_graph::audit::plan_available_partitioned(
                    &self.root,
                    &self.config.vault.dirs,
                    &pages,
                    self.config.graph.audit.batch_size,
                    self.config.graph.audit.max_input_bytes,
                    &mut |candidate| {
                        let reasons = readiness
                            .unfinished(&self.root, candidate)
                            .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
                        if reasons.is_empty() {
                            return Ok(true);
                        }
                        deferred.push(DeferredAudit {
                            path: candidate.path.clone(),
                            reasons,
                        });
                        Ok(false)
                    },
                )
                .map_err(|error| miette::miette!("{error}"))?;
                self.result.audit_deferred = deferred;
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
                readiness.require_ready(&self.root, candidate)?;
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
            let mut readiness = super::super::queue::readiness::AuditReadiness::load(&self.root)?;
            let mut deferred = Vec::new();
            let plan = lk_graph::audit::admit_available_partitioned(
                &session,
                &self.config.vault.dirs,
                &pages,
                self.config.graph.audit.batch_size,
                self.config.graph.audit.max_input_bytes,
                &mut |candidate| {
                    let reasons = readiness
                        .unfinished(&self.root, candidate)
                        .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
                    if reasons.is_empty() {
                        return Ok(true);
                    }
                    deferred.push(DeferredAudit {
                        path: candidate.path.clone(),
                        reasons,
                    });
                    Ok(false)
                },
                |candidates, mutation| {
                    let mut selected =
                        super::super::queue::readiness::AuditReadiness::load(&self.root)
                            .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
                    for candidate in candidates {
                        selected
                            .require_ready(&self.root, candidate)
                            .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
                    }
                    selected
                        .guard(&self.root, mutation)
                        .map_err(|error| lk_graph::GraphError::Io(error.to_string()))
                },
            )
            .map_err(|error| miette::miette!("{error}"))?;
            self.result.audit_deferred = deferred;
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
            let mut readiness = super::super::queue::readiness::AuditReadiness::load(&self.root)?;
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
                    match self.bounded_step(work, "lore-wiki").await {
                        WorkStep::Pending => continue,
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
            root: config.vault.root_path(),
            config,
            config_revision,
            configuration_failed: false,
            opts: options,
            supervisor,
            lease: None,
            selected: None,
            drain_blocked: false,
            audit_batch: None,
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
                audit: None,
                audit_progress: None,
                audit_blocked: Vec::new(),
                audit_blocked_count: 0,
                audit_deferred: Vec::new(),
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
}
