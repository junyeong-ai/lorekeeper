use super::*;

impl Runner<'_> {
    pub(super) fn current_audit_subjects(
        &self,
        batch: &[lk_graph::audit::AuditCandidate],
    ) -> miette::Result<Vec<lk_graph::audit::AuditCandidate>> {
        let pages = lk_graph::scan::scan_vault(&self.root, self.config.graph.scope.follow_links)
            .map_err(|error| miette::miette!("{error}"))?;
        let workflow = self
            .lease
            .as_ref()
            .ok_or_else(|| miette::miette!("audit has no workflow lease"))?;
        lk_graph::audit::resolve_batch_subjects(
            &self.root,
            &self.config.vault.dirs,
            &pages,
            batch,
            workflow.id(),
        )
        .map_err(|error| miette::miette!("{error}"))
    }

    pub(super) async fn whole_audit(&mut self, batch: &[lk_graph::audit::AuditCandidate]) {
        use crate::commands::work::audit::{capture, whole_request};
        use execution::audit::Judgment;
        let mut pending = (0..batch.len()).collect::<std::collections::BTreeSet<_>>();
        let mut replanned = std::collections::BTreeSet::new();
        while !pending.is_empty() {
            if !self.call_budget_available() || !self.configuration_is_current() {
                return;
            }
            let originals = pending
                .iter()
                .map(|index| batch[*index].clone())
                .collect::<Vec<_>>();
            let current = match self.current_audit_subjects(&originals) {
                Ok(current) => current,
                Err(error) => {
                    self.failure("audit fresh scope", error);
                    return;
                }
            };
            let mut readiness = match InputAvailability::load(&self.root) {
                Ok(readiness) => readiness,
                Err(error) => {
                    self.failure("audit input readiness", error);
                    return;
                }
            };
            let mut subjects = Vec::<crate::commands::work::audit::WholeSubject>::new();
            let mut slots = Vec::<Vec<usize>>::new();
            let mut missing = Vec::new();
            for (index, candidate) in pending
                .iter()
                .copied()
                .collect::<Vec<_>>()
                .into_iter()
                .zip(current)
            {
                match readiness.check(&candidate) {
                    Ok(true) => {}
                    Ok(false) => {
                        self.deferred("audit input dependencies", &candidate.path);
                        pending.remove(&index);
                        continue;
                    }
                    Err(error) => {
                        self.failure("audit input readiness", error);
                        return;
                    }
                }
                if candidate.discovery.is_none() {
                    missing.push(candidate.path);
                    continue;
                }
                if let Some(position) = subjects
                    .iter()
                    .position(|subject| subject.candidate.path == candidate.path)
                {
                    slots[position].push(index);
                    continue;
                }
                let subject =
                    match capture(&self.root, &self.config, &self.config_revision, &candidate) {
                        Ok(subject) => subject,
                        Err(error) => {
                            self.failure("audit input capture", error);
                            return;
                        }
                    };
                if let Some(reason) = subject.hold() {
                    self.deferred(
                        "whole audit action",
                        format!("{}: {reason}", candidate.path),
                    );
                    pending.remove(&index);
                    continue;
                }
                subjects.push(subject);
                slots.push(vec![index]);
            }
            if !missing.is_empty() {
                missing.sort();
                missing.dedup();
                if self.replan_audit(&missing).await.is_none() {
                    return;
                }
                continue;
            }
            if subjects.is_empty() {
                break;
            }
            let paths = subjects
                .iter()
                .map(|subject| subject.candidate.path.clone())
                .collect::<Vec<_>>();
            let selected =
                match lk_graph::scan::scan_vault(&self.root, self.config.graph.scope.follow_links)
                    .map_err(|error| miette::miette!("{error}"))
                    .and_then(|pages| {
                        lk_graph::audit::plan_selected_partitioned(
                            &self.root,
                            &self.config.vault.dirs,
                            &pages,
                            paths.len(),
                            &paths,
                            self.config.graph.audit.max_input_bytes,
                        )
                        .map_err(|error| miette::miette!("{error}"))
                    }) {
                    Ok(selected) => selected,
                    Err(error) => {
                        self.failure("audit current input bound", error);
                        return;
                    }
                };
            let mut bounded = selected.input.ensure_ready().is_err();
            let required = subjects
                .iter()
                .map(|subject| {
                    crate::commands::work::audit_needs_work(
                        &self.root,
                        &self.config,
                        &self.config_revision,
                        &subject.candidate,
                    )
                })
                .collect::<miette::Result<Vec<_>>>();
            let required = match required {
                Ok(required) => required,
                Err(error) => {
                    self.failure("audit request planning", error);
                    return;
                }
            };
            bounded |= required[0];
            if bounded {
                let Some(jobs) = self
                    .prepare_audit_jobs(&[subjects[0].candidate.clone()])
                    .await
                else {
                    return;
                };
                let Some(work) = jobs.get(&subjects[0].candidate.path) else {
                    for index in &slots[0] {
                        pending.remove(index);
                    }
                    continue;
                };
                let outcome = loop {
                    if !self.call_budget_available() {
                        return;
                    }
                    match self.bounded_step(work, ExecutionStage::Audit).await {
                        WorkStep::Pending => continue,
                        outcome => break outcome,
                    }
                };
                match outcome {
                    WorkStep::Staged => {
                        let assessment = self
                            .root
                            .join(".lorekeeper/work")
                            .join(work)
                            .join("assessment.json");
                        let Some(path) = assessment.to_str() else {
                            self.failure("audit publication", "assessment path is not UTF-8");
                            return;
                        };
                        if !self
                            .native(
                                "bounded audit publication",
                                &["wiki", "audit-record", "--file", path],
                            )
                            .await
                        {
                            return;
                        }
                        for index in &slots[0] {
                            self.audit_batch
                                .get_or_insert_with(Vec::new)
                                .push(batch[*index].clone());
                            pending.remove(index);
                        }
                    }
                    WorkStep::Replan => {
                        for index in &slots[0] {
                            if !replanned.insert(*index) {
                                self.deferred("audit inputs unstable", &batch[*index].path);
                                pending.remove(index);
                            }
                        }
                    }
                    WorkStep::Deferred(reason) => {
                        self.deferred("bounded audit action", reason);
                        for index in &slots[0] {
                            pending.remove(index);
                        }
                    }
                    WorkStep::Failed => return,
                    WorkStep::Pending => unreachable!("pending steps continue above"),
                }
                continue;
            }
            let mut count = 0;
            let mut request = None;
            for length in 1..=subjects.len() {
                if required[length - 1] {
                    break;
                }
                let candidate = whole_request(&self.config, &subjects[..length]);
                let candidate = match candidate {
                    Ok(candidate) => candidate,
                    Err(error)
                        if error
                            .downcast_ref::<crate::commands::work::generation::ResultCapacity>()
                            .is_some()
                            && count > 0 =>
                    {
                        break;
                    }
                    Err(error) => {
                        self.failure("whole audit request", error);
                        return;
                    }
                };
                if serde_json::to_vec(&candidate)
                    .expect("audit request serializes")
                    .len()
                    > self
                        .config
                        .execution
                        .effective_work()
                        .expect("validated work policy")
                        .max_input_bytes
                {
                    break;
                }
                count = length;
                request = Some(candidate);
            }
            let Some(request) = request else {
                self.failure(
                    "whole audit request",
                    "selected subject requires bounded Work",
                );
                return;
            };
            subjects.truncate(count);
            slots.truncate(count);
            let mut metrics = WorkStepMetrics::new(WorkRole::Finalizer);
            metrics.stages = vec![ExecutionStage::Audit];
            let Some(response) = self
                .request_result(&request, &execution::audit::schema(), &mut metrics)
                .await
            else {
                self.record_work_step(metrics);
                return;
            };
            let judgments = match execution::audit::decode_batch(response, count) {
                Ok(judgments) => judgments,
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("whole audit response", error);
                    return;
                }
            };
            let started = Instant::now();
            let Some((session, _)) = self.mutation_session("whole audit publication").await else {
                self.record_work_step(metrics);
                return;
            };
            let originals = slots
                .iter()
                .map(|indices| batch[indices[0]].clone())
                .collect::<Vec<_>>();
            let fresh = match self.current_audit_subjects(&originals) {
                Ok(fresh) => fresh,
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("audit fresh scope", error);
                    return;
                }
            };
            let queue_claim = match crate::commands::queue::claim_writer(
                &self.root.join(".lorekeeper/queue"),
                false,
            ) {
                Ok(claim) => claim,
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("audit input readiness", error);
                    return;
                }
            };
            let mut readiness = match InputAvailability::load(&self.root) {
                Ok(readiness) => readiness,
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("audit input readiness", error);
                    return;
                }
            };
            let mut action_readiness = match InputAvailability::load(&self.root) {
                Ok(readiness) => readiness,
                Err(error) => {
                    self.record_work_step(metrics);
                    self.failure("audit action readiness", error);
                    return;
                }
            };
            let mut actions = lk_vault::MutationPlan::default();
            let mut held = 0;
            let mut completed = Vec::new();
            for (((subject, indices), candidate), judgment) in
                subjects.iter().zip(&slots).zip(fresh).zip(judgments)
            {
                let fresh =
                    match capture(&self.root, &self.config, &self.config_revision, &candidate) {
                        Ok(fresh) => fresh,
                        Err(error) => {
                            self.record_work_step(metrics);
                            self.failure("audit input revalidation", error);
                            return;
                        }
                    };
                if !subject.same_input(&fresh) {
                    for index in indices {
                        if !replanned.insert(*index) {
                            self.deferred("audit inputs unstable", &batch[*index].path);
                            pending.remove(index);
                        }
                    }
                    self.record("whole audit replanning", StageState::Skipped, Some("selected inputs changed; discard the old judgment and reread current inputs within the same run budget".into()));
                    continue;
                }
                match readiness.check(&candidate) {
                    Ok(true) => {}
                    Ok(false) => {
                        self.deferred("audit input dependencies", &candidate.path);
                        for index in indices {
                            pending.remove(index);
                        }
                        continue;
                    }
                    Err(error) => {
                        self.record_work_step(metrics);
                        self.failure("audit input readiness", error);
                        return;
                    }
                }
                match judgment {
                    Judgment::Complete {
                        disposition,
                        reason,
                    } => {
                        let assessment = match fresh.assessment(disposition, reason) {
                            Ok(assessment) => assessment,
                            Err(error) => {
                                self.record_work_step(metrics);
                                self.failure("audit assembly", error);
                                return;
                            }
                        };
                        completed.push((assessment, fresh, indices));
                    }
                    Judgment::NeedsAction(reason) => {
                        if let Err(error) = action_readiness.require_ready(&candidate) {
                            self.record_work_step(metrics);
                            self.failure("audit action readiness", error);
                            return;
                        }
                        if let Err(error) =
                            fresh.plan_hold(&self.root, reason.clone(), &mut actions)
                        {
                            self.record_work_step(metrics);
                            self.failure("whole audit action", error);
                            return;
                        }
                        held += 1;
                        self.deferred(
                            "whole audit action",
                            format!("{}: {reason}", candidate.path),
                        );
                        for index in indices {
                            pending.remove(index);
                        }
                    }
                }
            }
            if let Err(error) = action_readiness
                .guard(&mut actions)
                .and_then(|()| session.apply(actions).into_diagnostic())
            {
                self.record_work_step(metrics);
                self.failure("whole audit action publication", error);
                return;
            }
            drop(queue_claim);
            self.result.work_telemetry.retained_progress.action_receipts += held;
            if !completed.is_empty() {
                let results = lk_graph::audit::AuditResults {
                    assessments: completed
                        .iter()
                        .map(|(assessment, _, _)| assessment.clone())
                        .collect(),
                };
                let recorded = crate::commands::audit::record(
                    &session,
                    &self.config,
                    &self.config_revision,
                    results,
                    |assessment, mutation| {
                        completed
                            .iter()
                            .find(|(result, _, _)| result.path == assessment.path)
                            .expect("assembled assessment belongs to captured input")
                            .1
                            .guard(mutation);
                        Ok(())
                    },
                );
                if let Err(error) = recorded {
                    self.record_work_step(metrics);
                    self.failure("whole audit publication", error);
                    return;
                }
                for (_, _, indices) in completed {
                    for index in indices {
                        self.audit_batch
                            .get_or_insert_with(Vec::new)
                            .push(batch[*index].clone());
                        pending.remove(index);
                    }
                }
                metrics.outcome = WorkOutcome::Staged;
                self.record("whole audit publication", StageState::Completed, None);
            }
            metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
            self.record_work_step(metrics);
        }
        if self.audit_batch.as_ref().is_none_or(Vec::is_empty) {
            self.result.audit = Some(Vec::new());
        }
    }
}
