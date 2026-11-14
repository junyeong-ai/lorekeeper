use super::*;

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
    pub(super) async fn discover_extraction(&mut self, task: &lk_queue::QueueTask) -> bool {
        let subject = match DiscoverySubject::load(
            &self.root,
            &task.target.vault_path,
            self.supervisor.read_control(),
        ) {
            Ok(subject) => subject,
            Err(error) => return self.failure("extraction discovery input", error),
        };
        let limit = self
            .config
            .execution
            .effective_work()
            .expect("validated work policy")
            .max_result_bytes;
        let mut input = subject.input(0);
        for key in ["focus", "source_type", "concept_policy"] {
            if let Some(value) = task.input.get(key) {
                input[key] = value.clone();
            }
        }
        let request = execution::preparation::discovery_request(
            &[input],
            execution::preparation::DiscoveryKind::Extraction,
            limit,
        );
        let mut metrics = WorkStepMetrics::new(WorkRole::Discovery);
        metrics.stages.push(ExecutionStage::Discovery);
        let response = self
            .request_result(
                &request,
                &execution::preparation::discovery_schema(),
                &mut metrics,
            )
            .await;
        let Some(response) = response else {
            self.record_work_step(metrics);
            return false;
        };
        let queries = match execution::preparation::discovery_queries(
            response,
            1,
            execution::preparation::DiscoveryKind::Extraction,
        ) {
            Ok(mut queries) => queries.remove(0),
            Err(error) => {
                self.record_work_step(metrics);
                return self.failure("extraction discovery response", error);
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
                self.queue_replan = true;
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
        let inputs = subjects
            .iter()
            .enumerate()
            .map(|(index, subject)| subject.input(index))
            .collect::<Vec<_>>();
        let limit = self
            .config
            .execution
            .effective_work()
            .expect("validated work policy")
            .max_result_bytes;
        let request = execution::preparation::discovery_request(
            &inputs,
            execution::preparation::DiscoveryKind::Audit,
            limit,
        );
        let mut metrics = WorkStepMetrics::new(WorkRole::Discovery);
        metrics.stages.push(ExecutionStage::Discovery);
        let response = self
            .request_result(
                &request,
                &execution::preparation::discovery_schema(),
                &mut metrics,
            )
            .await;
        let Some(response) = response else {
            self.record_work_step(metrics);
            return false;
        };
        let queries = match execution::preparation::discovery_queries(
            response,
            subjects.len(),
            execution::preparation::DiscoveryKind::Audit,
        ) {
            Ok(queries) => queries,
            Err(error) => {
                self.record_work_step(metrics);
                return self.failure("audit discovery response", error);
            }
        };
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
        for (subject, queries) in subjects.into_iter().zip(queries) {
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
            let changed = match lk_graph::audit::record_discovery_if_missing(
                &session,
                &self.config.vault.dirs,
                &pages,
                output,
                self.supervisor.read_control(),
            ) {
                Ok(changed) => changed,
                Err(error) => {
                    self.record_work_step(metrics);
                    return self.failure("audit discovery recording", error);
                }
            };
            self.result
                .work_telemetry
                .retained_progress
                .discovery_recipes += usize::from(changed);
        }
        metrics.outcome = WorkOutcome::Recorded;
        metrics.revalidation_record_duration_ms = Some(started.elapsed().as_millis() as u64);
        self.record_work_step(metrics);
        true
    }
}
