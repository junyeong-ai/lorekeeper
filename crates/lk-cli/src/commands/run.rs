use clap::ValueEnum;
use serde::Serialize;

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
    use super::*;
    use crate::commands::{CONFIG_REVISION_ENV, GlobalOptions, execution, workflow};
    use lk_core::config::Config;
    use lk_core::execution::ExecutionProfile;
    use std::path::PathBuf;
    use std::time::Duration;

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
    }

    #[derive(Default, Serialize)]
    struct ReviewHealth {
        open: usize,
        resolved: usize,
        needs_reassessment: usize,
        invalid: usize,
    }

    #[derive(Serialize)]
    struct JobResult {
        operation: Operation,
        ok: bool,
        cancelled: bool,
        execution: Option<ExecutionInfo>,
        workflow_id: Option<lk_core::execution::WorkflowId>,
        audit: Option<Vec<lk_graph::audit::AuditCompletion>>,
        audit_blocked: Vec<lk_graph::audit::AuditBlockedInput>,
        audit_blocked_count: usize,
        review_health: Option<ReviewHealth>,
        stages: Vec<StageResult>,
    }

    struct Runner<'a> {
        opts: &'a GlobalOptions,
        config: Config,
        config_revision: String,
        configuration_failed: bool,
        root: PathBuf,
        profile: Option<String>,
        supervisor: &'a mut execution::Supervisor,
        lease: Option<workflow::WorkflowLease>,
        selected: Option<ExecutionProfile>,
        drain_blocked: bool,
        result: JobResult,
    }

    impl Runner<'_> {
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
                .resolve_profile(self.result.operation.semantic(), self.profile.as_deref())
                .map_err(|e| miette::miette!("{e}"))?;
            let name = name.to_owned();
            let mut profile = profile.clone();
            let executable = execution::executable_path(profile.executable())?;
            match &mut profile {
                ExecutionProfile::ClaudeCode {
                    executable: path, ..
                }
                | ExecutionProfile::Codex {
                    executable: path, ..
                } => *path = executable.clone(),
            }
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
            });
            self.selected = Some(profile);
            Ok(())
        }

        async fn consumer(&mut self, skill: &str, operation: &str) -> bool {
            if self.result.cancelled || !self.configuration_is_current() {
                return false;
            }
            if self.config.llm.provider == lk_core::config::LlmProvider::Noop {
                return self.failure(skill, "semantic execution is disabled by llm.provider");
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
                    )
                });
            match command {
                Ok(mut command) => {
                    command.current_dir(&self.root);
                    self.child(skill, command, Some(timeout)).await
                }
                Err(error) => self.failure(skill, error),
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
                    let operation = if matches!(self.result.operation, Operation::Knowledge) {
                        "Process current queue tasks as part of this bounded knowledge maintenance job. Read the selected work's review dependencies. The native controller inspects and reports global review health after this session; do not enumerate unrelated reviews or expand the batch to repair them."
                    } else {
                        ""
                    };
                    let completed = self.consumer("lore-process", operation).await;
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

        fn audit_plan(&self, paths: &[String]) -> miette::Result<lk_graph::audit::AuditPlan> {
            let pages =
                lk_graph::scan::scan_vault(&self.root, self.config.graph.scope.follow_links)
                    .map_err(|e| miette::miette!("{e}"))?;
            lk_graph::audit::plan_selected(
                &self.root,
                &self.config.vault.dirs,
                &pages,
                self.config.graph.audit.batch_size,
                paths,
                self.config.graph.audit.max_input_bytes,
            )
            .map_err(|e| miette::miette!("{e}"))
        }

        fn verify_audit(
            &self,
            batch: &[lk_graph::audit::AuditCandidate],
        ) -> miette::Result<Vec<lk_graph::audit::AuditCompletion>> {
            let pages =
                lk_graph::scan::scan_vault(&self.root, self.config.graph.scope.follow_links)
                    .map_err(|e| miette::miette!("{e}"))?;
            let workflow = self
                .lease
                .as_ref()
                .ok_or_else(|| miette::miette!("audit has no workflow lease"))?;
            lk_graph::audit::verify_batch(
                &self.root,
                &self.config.vault.dirs,
                &pages,
                batch,
                workflow.id(),
                self.config.graph.audit.max_input_bytes,
            )
            .map_err(|e| miette::miette!("{e}"))
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
            self.reconcile().await;
            if !self.result.ok || self.result.cancelled {
                return;
            }
            if !self.inspect_review_health() {
                return;
            }
            let plan = match self.audit_plan(&[]) {
                Ok(plan) => plan,
                Err(error) => {
                    self.failure("audit planning", error);
                    return;
                }
            };
            if let Err(error) = plan.input.ensure_ready() {
                self.failure("audit input bound", error);
                return;
            }
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
                self.result.audit = Some(Vec::new());
                if plan.blocked_count > 0 {
                    return;
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
            self.consumer("lore-wiki", &format!("maintain. The controller selected this exact batch: {selection}. Start with audit-plan --path for these addresses; do not select another batch.")).await;
            self.reconcile().await;
            if !self.inspect_review_health() {
                return;
            }
            match self.verify_audit(&plan.candidates) {
                Ok(completed) => {
                    self.result.audit = Some(completed);
                    self.record("audit completion", StageState::Completed, None);
                }
                Err(error) => {
                    self.failure("audit completion", error);
                }
            }
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
        if batches == 0 || batches != 1 && !matches!(operation, Operation::Knowledge) {
            return Err(miette::miette!(
                "multiple batches are only supported for knowledge jobs"
            ));
        }
        let config_path = crate::commands::find_config(opts)?;
        let (config, config_revision) = crate::commands::load_config_with_revision(&config_path)?;
        if let Some(name) = profile.as_deref() {
            config
                .execution
                .resolve_profile(operation.semantic(), Some(name))
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
                profile.clone(),
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
            } else if result.audit.as_ref().is_some_and(Vec::is_empty) {
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
        profile: Option<String>,
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
            profile,
            supervisor,
            lease: None,
            selected: None,
            drain_blocked: false,
            result: JobResult {
                operation,
                ok: true,
                cancelled: false,
                execution: None,
                workflow_id: None,
                audit: None,
                audit_blocked: Vec::new(),
                audit_blocked_count: 0,
                review_health: None,
                stages: Vec::new(),
            },
        };
        if !runner.configuration_is_current() {
            return runner.result;
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
        runner.result
    }
}
