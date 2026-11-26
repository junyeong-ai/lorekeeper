use super::*;
use crate::commands::execution::preparation::Outcome;
use crate::commands::work::preparation::{Binding, Disposition};

struct DiscoverySubject {
    path: String,
    revision: String,
}

impl DiscoverySubject {
    fn load(root: &Path, path: &str, read: &lk_vault::ReadControl) -> miette::Result<Self> {
        let absolute = lk_vault::page_path(root, Path::new(path)).into_diagnostic()?;
        let raw = read.read_to_string(&absolute).into_diagnostic()?;
        Ok(Self {
            path: path.into(),
            revision: lk_core::concept::file_revision(&raw),
        })
    }

    fn input(&self, index: usize) -> serde_json::Value {
        serde_json::json!({"subject_ref":format!("s{index}"),"path":self.path,"file_revision":self.revision})
    }

    fn is_current(&self, root: &Path, read: &lk_vault::ReadControl) -> miette::Result<bool> {
        Ok(Self::load(root, &self.path, read)?.revision == self.revision)
    }
}

impl Runner<'_> {
    pub(super) fn invalid_preparation_response(
        &self,
        response: &serde_json::Value,
        metrics: &WorkStepMetrics,
    ) -> Disposition {
        let bytes = serde_json::to_vec(response).expect("decoded response serializes");
        Disposition::invocation_failed(
            crate::commands::work::state::InvocationFailure::InvalidResponse {
                engine: self.agent_profile(metrics).engine(),
                revision: lk_core::rss_original::digest(&bytes),
                bytes: bytes.len(),
                host: metrics.host_end.clone(),
            },
        )
    }

    pub(super) async fn hold_audit_screening(
        &mut self,
        path: &str,
        request: &lk_graph::audit::selection::ScreeningRequest,
        binding: &Binding,
        disposition: Disposition,
        metrics: &mut WorkStepMetrics,
    ) -> bool {
        let started = Instant::now();
        let Some((session, _)) = self.mutation_session("audit screening attempt").await else {
            return false;
        };
        let recorded = (|| -> miette::Result<bool> {
            let fresh = self.audit_plan(&[path.into()])?;
            let Some(candidate) = fresh
                .candidates
                .iter()
                .find(|candidate| candidate.path == path)
            else {
                return Ok(false);
            };
            let mut plan = binding.plan_record(session.root(), disposition.clone())?;
            match lk_graph::audit::selection::guard_packet(
                session.root(),
                &self.config.vault.dirs,
                candidate,
                request,
                &mut plan,
                self.supervisor.read_control(),
            )
            .map_err(|error| miette::miette!("{error}"))?
            {
                lk_graph::audit::selection::PacketStatus::Unrecorded => {}
                lk_graph::audit::selection::PacketStatus::Recorded
                | lk_graph::audit::selection::PacketStatus::Changed => return Ok(false),
            }
            self.supervisor.read_control().check().into_diagnostic()?;
            session.apply(plan).into_diagnostic()?;
            Ok(true)
        })();
        drop(session);
        metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
        match recorded {
            Ok(true) => {
                metrics.outcome = if matches!(disposition, Disposition::NeedsAction { .. }) {
                    WorkOutcome::Rejected
                } else {
                    WorkOutcome::Failed
                };
                self.result.work_telemetry.retained_progress.action_receipts +=
                    usize::from(matches!(disposition, Disposition::NeedsAction { .. }));
                let reason = match binding.held(&self.root) {
                    Ok(Some(reason)) => reason,
                    Ok(None) => {
                        return self.failure(
                            "audit attempt verification",
                            "recorded preparation hold changed before verification",
                        );
                    }
                    Err(error) => return self.failure("audit attempt verification", error),
                };
                self.audit_screening_deferred
                    .insert(path.into(), reason.clone());
                self.deferred("audit screening input held", reason);
                true
            }
            Ok(false) => {
                metrics.outcome = WorkOutcome::Rejected;
                self.deferred(
                    "audit screening changed",
                    "the returned failure no longer names an unrecorded current packet",
                );
                false
            }
            Err(error) => self.failure("audit screening attempt", error),
        }
    }

    fn extraction_discovery_request(
        &self,
        task: &lk_queue::QueueTask,
        subject: &DiscoverySubject,
    ) -> (usize, serde_json::Value) {
        let budget = self
            .config
            .execution
            .effective_work()
            .expect("validated work policy");
        let mut input = subject.input(0);
        for key in ["focus", "source_type", "concept_policy"] {
            if let Some(value) = task.input.get(key) {
                input[key] = value.clone();
            }
        }
        execution::preparation::discovery_batch(
            &[input],
            execution::preparation::DiscoveryKind::Extraction,
            budget.max_input_bytes,
            budget.max_result_bytes,
        )
    }

    pub(super) fn extraction_discovery_hold(
        &self,
        task: &lk_queue::QueueTask,
    ) -> miette::Result<Option<String>> {
        let subject = DiscoverySubject::load(
            &self.root,
            &task.target.vault_path,
            self.supervisor.read_control(),
        )?;
        if self.extraction_discovery_request(task, &subject).0 == 0 {
            return Ok(Some("complete extraction discovery request or its minimum response exceeds the configured byte allowance".into()));
        }
        Binding::extraction_discovery(
            task,
            &subject.revision,
            &serde_json::json!({}),
            &serde_json::json!({}),
        )?
        .held(&self.root)
    }

    async fn hold_discovery(
        &mut self,
        subject: &DiscoverySubject,
        binding: &Binding,
        task: Option<&lk_queue::QueueTask>,
        disposition: Disposition,
        metrics: &mut WorkStepMetrics,
    ) -> bool {
        let started = Instant::now();
        let Some((session, _)) = self.mutation_session("discovery attempt recording").await else {
            return false;
        };
        let recorded = (|| -> miette::Result<bool> {
            let _queue = task
                .map(|_| {
                    crate::commands::queue::claim_writer(
                        &session.root().join(".lorekeeper/queue"),
                        false,
                    )
                })
                .transpose()?;
            let mut plan = binding.plan_record(session.root(), disposition.clone())?;
            let absolute =
                lk_vault::page_path(session.root(), Path::new(&subject.path)).into_diagnostic()?;
            let raw = self
                .supervisor
                .read_control()
                .read_to_string(&absolute)
                .into_diagnostic()?;
            if lk_core::concept::file_revision(&raw) != subject.revision {
                return Ok(false);
            }
            plan.require_unchanged(Path::new(&subject.path), raw);
            if let Some(task) = task {
                crate::commands::queue::guard_current_task(session.root(), task, &mut plan)?;
            }
            self.supervisor.read_control().check().into_diagnostic()?;
            session.apply(plan).into_diagnostic()?;
            Ok(true)
        })();
        drop(session);
        metrics.revalidation_record_duration_ms = Some(
            metrics.revalidation_record_duration_ms.unwrap_or(0)
                + started.elapsed().as_millis() as u64,
        );
        match recorded {
            Ok(true) => {
                metrics.outcome = if matches!(disposition, Disposition::NeedsAction { .. }) {
                    WorkOutcome::Rejected
                } else {
                    WorkOutcome::Failed
                };
                self.result.work_telemetry.retained_progress.action_receipts +=
                    usize::from(matches!(disposition, Disposition::NeedsAction { .. }));
                let reason = match binding.held(&self.root) {
                    Ok(Some(reason)) => reason,
                    Ok(None) => {
                        return self.failure(
                            "discovery attempt verification",
                            "recorded preparation hold changed before verification",
                        );
                    }
                    Err(error) => return self.failure("discovery attempt verification", error),
                };
                if task.is_none() {
                    self.audit_screening_deferred
                        .insert(subject.path.clone(), reason.clone());
                }
                self.deferred(
                    "discovery input held",
                    format!("{}: {reason}", subject.path),
                );
                true
            }
            Ok(false) => {
                self.deferred("discovery input changed", &subject.path);
                false
            }
            Err(error) => self.failure("discovery attempt recording", error),
        }
    }

    pub(super) async fn discover_extraction(&mut self, task: &lk_queue::QueueTask) -> bool {
        let subject = match DiscoverySubject::load(
            &self.root,
            &task.target.vault_path,
            self.supervisor.read_control(),
        ) {
            Ok(subject) => subject,
            Err(error) => return self.failure("extraction discovery input", error),
        };
        let (count, request) = self.extraction_discovery_request(task, &subject);
        if count == 0 {
            return self.deferred(
                "extraction discovery capacity",
                "complete request or its minimum response exceeds the configured byte allowance",
            );
        }
        let schema = execution::preparation::discovery_schema();
        let binding =
            match Binding::extraction_discovery(task, &subject.revision, &request, &schema) {
                Ok(binding) => binding,
                Err(error) => return self.failure("extraction discovery binding", error),
            };
        match binding.held(&self.root) {
            Ok(Some(reason)) => {
                self.deferred("extraction discovery held", reason);
                return false;
            }
            Ok(None) => {}
            Err(error) => return self.failure("extraction discovery hold", error),
        }
        let mut metrics = WorkStepMetrics::new(WorkRole::Discovery);
        metrics.stages.push(ExecutionStage::Discovery);
        let response = self.request_result(&request, &schema, &mut metrics).await;
        let outcome = match response {
            Some(ResultReply::InvocationFailed(failure)) => {
                Err(Disposition::invocation_failed(failure))
            }
            Some(ResultReply::Complete(response)) => {
                match execution::preparation::discovery_outcomes(
                    response.clone(),
                    1,
                    execution::preparation::DiscoveryKind::Extraction,
                ) {
                    Ok(mut outcomes) => match outcomes.remove(0) {
                        Outcome::Complete(queries) => Ok(queries),
                        Outcome::NeedsAction(reason) => Err(Disposition::NeedsAction { reason }),
                    },
                    Err(_) => Err(self.invalid_preparation_response(&response, &metrics)),
                }
            }
            None => {
                self.record_work_step(metrics);
                return false;
            }
        };
        let queries = match outcome {
            Ok(queries) => queries,
            Err(disposition) => {
                if self
                    .hold_discovery(&subject, &binding, Some(task), disposition, &mut metrics)
                    .await
                {
                    self.replan_after_hold();
                }
                self.record_work_step(metrics);
                return false;
            }
        };
        let started = Instant::now();
        let Some((session, _)) = self
            .mutation_session("extraction discovery recording")
            .await
        else {
            self.record_work_step(metrics);
            return false;
        };
        match subject.is_current(session.root(), self.supervisor.read_control()) {
            Ok(true) => {}
            Ok(false) => {
                metrics.outcome = WorkOutcome::Rejected;
                self.record_work_step(metrics);
                self.queue_replan = QueueReplan::Invalidate;
                return true;
            }
            Err(error) => {
                self.record_work_step(metrics);
                return self.failure("extraction discovery input", error);
            }
        }
        let recorded = crate::commands::work::record_native_discovery(
            &session,
            task,
            subject.revision,
            queries,
        );
        drop(session);
        match recorded {
            Ok(changed) => {
                metrics.outcome = if changed {
                    WorkOutcome::Recorded
                } else {
                    WorkOutcome::AlreadyComplete
                };
                metrics.revalidation_record_duration_ms =
                    Some(started.elapsed().as_millis() as u64);
                self.result
                    .work_telemetry
                    .retained_progress
                    .discovery_recipes += usize::from(changed);
                self.record_work_step(metrics);
                true
            }
            Err(error) => {
                self.record_work_step(metrics);
                self.failure("extraction discovery recording", error)
            }
        }
    }

    pub(super) async fn discover_audit(&mut self, paths: &[String]) -> bool {
        let subjects = paths
            .iter()
            .map(|path| DiscoverySubject::load(&self.root, path, self.supervisor.read_control()))
            .collect::<miette::Result<Vec<_>>>();
        let subjects = match subjects {
            Ok(subjects) => subjects,
            Err(error) => return self.failure("audit discovery input", error),
        };
        let schema = execution::preparation::discovery_schema();
        let limit = self
            .config
            .execution
            .effective_work()
            .expect("validated work policy")
            .max_result_bytes;
        let mut available = Vec::new();
        for subject in subjects {
            let binding = match Binding::audit_discovery(
                &subject.path,
                &subject.revision,
                &serde_json::json!({}),
                &schema,
            ) {
                Ok(binding) => binding,
                Err(error) => return self.failure("audit discovery binding", error),
            };
            match binding.held(&self.root) {
                Ok(Some(reason)) => {
                    self.audit_screening_deferred
                        .insert(subject.path.clone(), reason.clone());
                    self.deferred("audit discovery held", reason);
                }
                Ok(None) => available.push(subject),
                Err(error) => return self.failure("audit discovery hold", error),
            }
        }
        let mut subjects = available;
        if subjects.is_empty() {
            return false;
        }
        let inputs = subjects
            .iter()
            .enumerate()
            .map(|(index, subject)| subject.input(index))
            .collect::<Vec<_>>();
        let (count, request) = execution::preparation::discovery_batch(
            &inputs,
            execution::preparation::DiscoveryKind::Audit,
            self.config
                .execution
                .effective_work()
                .expect("validated work policy")
                .max_input_bytes,
            limit,
        );
        if count == 0 {
            let reason = "one complete discovery subject or its minimum response exceeds the configured byte allowance".to_string();
            self.audit_screening_deferred
                .insert(subjects[0].path.clone(), reason.clone());
            self.deferred(
                "audit discovery capacity",
                format!("{}: {reason}", subjects[0].path),
            );
            return true;
        }
        subjects.truncate(count);
        let bindings = subjects
            .iter()
            .map(|subject| {
                Binding::audit_discovery(&subject.path, &subject.revision, &request, &schema)
            })
            .collect::<miette::Result<Vec<_>>>();
        let bindings = match bindings {
            Ok(bindings) => bindings,
            Err(error) => return self.failure("audit discovery binding", error),
        };
        let mut metrics = WorkStepMetrics::new(WorkRole::Discovery);
        metrics.stages.push(ExecutionStage::Discovery);
        let response = self.request_result(&request, &schema, &mut metrics).await;
        let outcomes = match response {
            Some(ResultReply::Complete(response)) => {
                match execution::preparation::discovery_outcomes(
                    response.clone(),
                    subjects.len(),
                    execution::preparation::DiscoveryKind::Audit,
                ) {
                    Ok(outcomes) => outcomes
                        .into_iter()
                        .map(|outcome| match outcome {
                            Outcome::Complete(queries) => Ok(queries),
                            Outcome::NeedsAction(reason) => {
                                Err(Disposition::NeedsAction { reason })
                            }
                        })
                        .collect::<Vec<_>>(),
                    Err(_) => {
                        let disposition = self.invalid_preparation_response(&response, &metrics);
                        subjects.iter().map(|_| Err(disposition.clone())).collect()
                    }
                }
            }
            Some(ResultReply::InvocationFailed(failure)) => subjects
                .iter()
                .map(|_| Err(Disposition::invocation_failed(failure.clone())))
                .collect(),
            None => {
                self.record_work_step(metrics);
                return false;
            }
        };
        let mut complete = Vec::new();
        for ((subject, binding), outcome) in subjects.into_iter().zip(bindings).zip(outcomes) {
            match outcome {
                Ok(queries) => complete.push((subject, queries)),
                Err(disposition) => {
                    self.hold_discovery(&subject, &binding, None, disposition, &mut metrics)
                        .await;
                    if self.result.is_halted() || self.result.cancelled {
                        self.record_work_step(metrics);
                        return false;
                    }
                }
            }
        }
        if complete.is_empty() {
            self.record_work_step(metrics);
            return false;
        }
        let started = Instant::now();
        let Some((session, _)) = self.mutation_session("audit discovery recording").await else {
            self.record_work_step(metrics);
            return false;
        };
        let pages = match lk_graph::scan::scan_vault_controlled(
            session.root(),
            self.config.graph.scope.follow_links,
            self.supervisor.read_control(),
        ) {
            Ok(pages) => pages,
            Err(error) => {
                self.record_work_step(metrics);
                return self.failure("audit discovery recording", error);
            }
        };
        let mut usable = false;
        let mut recorded = 0;
        for (subject, queries) in complete {
            match subject.is_current(session.root(), self.supervisor.read_control()) {
                Ok(true) => {}
                Ok(false) => {
                    self.deferred("audit discovery changed", &subject.path);
                    continue;
                }
                Err(error) => {
                    self.record_work_step(metrics);
                    return self.failure("audit discovery input", error);
                }
            }
            let output = lk_graph::audit::AuditDiscoveryRequest {
                path: subject.path,
                file_revision: subject.revision,
                queries,
            };
            match lk_graph::audit::record_discovery_if_missing(
                &session,
                &self.config.vault.dirs,
                &pages,
                output,
                self.supervisor.read_control(),
            ) {
                Ok(changed) => {
                    recorded += usize::from(changed);
                    usable = true;
                }
                Err(error) => {
                    self.record_work_step(metrics);
                    return self.failure("audit discovery recording", error);
                }
            }
        }
        self.result
            .work_telemetry
            .retained_progress
            .discovery_recipes += recorded;
        metrics.outcome = if recorded > 0 {
            WorkOutcome::Recorded
        } else {
            WorkOutcome::AlreadyComplete
        };
        metrics.revalidation_record_duration_ms = Some(
            metrics.revalidation_record_duration_ms.unwrap_or(0)
                + started.elapsed().as_millis() as u64,
        );
        self.record_work_step(metrics);
        usable
    }
}
