use super::*;

pub(crate) struct ObservationTextRejection {
    pub output: WorkOutput,
    pub violation: ObservationTextError,
    pub response_revision: String,
    pub response_json_bytes: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservationRejections {
    schema: String,
    work_id: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    entries: BTreeMap<String, Rejection>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejection {
    node_id: String,
    input_revision: String,
    contract_revision: String,
    response_revision: String,
    response_json_bytes: usize,
    violation: ObservationTextError,
}

pub(crate) fn path(work: &Work) -> miette::Result<PathBuf> {
    work_path(&work.plan.id, "observation-rejections.json")
}

impl ObservationRejections {
    pub(crate) fn decode(raw: &str, work: &Work, nodes: &[Node]) -> miette::Result<Self> {
        if raw.len() > work.max_input_bytes {
            return Err(fail(
                "observation rejection history exceeds the work input bound",
            ));
        }
        let records: Self = crate::commands::work::receipt::decode(raw).map_err(fail)?;
        if records.schema != "observation-rejections-v1"
            || records.work_id != work.plan.id
            || records.entries.is_empty()
        {
            return Err(fail("invalid observation rejection identity or schema"));
        }
        for (request, entry) in &records.entries {
            for id in [
                request,
                &entry.node_id,
                &entry.input_revision,
                &entry.contract_revision,
                &entry.response_revision,
            ] {
                require_id(id)?;
            }
            if !nodes.iter().any(|node| node.id == entry.node_id)
                || entry.response_json_bytes == 0
                || entry.response_json_bytes > work.plan.max_result_bytes
                || entry.violation.index().is_some_and(|index| {
                    index
                        >= work
                            .max_input_bytes
                            .saturating_add(entry.response_json_bytes)
                })
            {
                return Err(fail(
                    "invalid observation rejection node, field or response bound",
                ));
            }
        }
        Ok(records)
    }

    pub(crate) fn load(
        root: &Path,
        work: &Work,
        nodes: &[Node],
    ) -> miette::Result<Option<(PathBuf, String, Self)>> {
        let path = path(work)?;
        read_optional(root, &path)?
            .map(|raw| {
                let records = Self::decode(&raw, work, nodes)?;
                Ok((path, raw, records))
            })
            .transpose()
    }

    pub(crate) fn has_node(&self, node: &str) -> bool {
        self.entries.values().any(|entry| entry.node_id == node)
    }

    pub(crate) fn reason(
        &self,
        work: &Work,
        request: &Value,
        contract: &str,
    ) -> miette::Result<Option<String>> {
        let Some(entry) = self.entries.get(&request_revision(request, contract)?) else {
            return Ok(None);
        };
        if request["work_id"] != work.plan.id
            || request["node_id"] != entry.node_id
            || request["input_revision"] != entry.input_revision
            || entry.contract_revision != contract
        {
            return Err(fail("observation rejection differs from its exact request"));
        }
        Ok(Some(format!(
            "{}; no observation was recorded",
            entry.violation
        )))
    }
}

impl Work {
    pub fn plan_observation_rejection(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        rejected: ObservationTextRejection,
    ) -> miette::Result<MutationPlan> {
        require_id(contract)?;
        require_id(&rejected.response_revision)?;
        let evaluated = self.evaluate(root, resources)?;
        let node = self.validate_output_node(&rejected.output, &evaluated)?;
        if !evaluated.ready.contains(&node.id) {
            return Err(fail(
                "observation rejection requires an unfinished ready node",
            ));
        }
        if self
            .node_input(node, &evaluated.inputs, &evaluated.current)
            .map_err(NodeInputError::report)?
            != *request
        {
            return Err(fail(
                "observation rejection request changed before recording",
            ));
        }
        let children = match &node.kind {
            NodeKind::Inspect { .. } => Vec::new(),
            NodeKind::Reduce { children } => children
                .iter()
                .map(|id| &evaluated.current[id].output)
                .collect(),
        };
        validate_output_structure(&rejected.output, node, &children)?;
        if rejected.output.validate_text().err().as_ref() != Some(&rejected.violation) {
            return Err(fail(
                "observation rejection does not identify its authored violation",
            ));
        }
        let target = path(self)?;
        let before = evaluated.raw.get(&target).cloned();
        let mut records = match &before {
            Some(raw) => ObservationRejections::decode(raw, self, &evaluated.nodes)?,
            None => ObservationRejections {
                schema: "observation-rejections-v1".into(),
                work_id: self.plan.id.clone(),
                entries: BTreeMap::new(),
            },
        };
        let key = request_revision(request, contract)?;
        if records.entries.contains_key(&key) {
            return Err(fail(
                "observation rejection already retained for this exact request",
            ));
        }
        records.entries.insert(
            key,
            Rejection {
                node_id: node.id.clone(),
                input_revision: rejected.output.input_revision,
                contract_revision: contract.into(),
                response_revision: rejected.response_revision,
                response_json_bytes: rejected.response_json_bytes,
                violation: rejected.violation,
            },
        );
        let after = encoded(&records)?;
        ObservationRejections::decode(&after, self, &evaluated.nodes)?;
        let mut plan = MutationPlan::default();
        if evaluated.refinements.is_none() {
            plan.require_absent(super::super::refinement::path(self)?);
        }
        let output = output_path(&self.plan.id, &node.id)?;
        if !evaluated.raw.contains_key(&output) {
            plan.require_absent(output);
        }
        for (path, raw) in evaluated.raw {
            if path != target {
                plan.require_unchanged(path, raw);
            }
        }
        plan.publish_expected(root, target, before, after)
            .map_err(fail)?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::{context, limits, resources};
    use super::*;

    fn reply(request: &Value) -> ObservationTextRejection {
        ObservationTextRejection {
            output: WorkOutput {
                work_id: request["work_id"].as_str().unwrap().into(),
                node_id: request["node_id"].as_str().unwrap().into(),
                input_revision: request["input_revision"].as_str().unwrap().into(),
                summary: " \u{2003}\t".into(),
                findings: Vec::new(),
                source_questions: Vec::new(),
                unresolved: Vec::new(),
                resolutions: Vec::new(),
            },
            violation: ObservationTextError::Summary,
            response_revision: lk_core::rss_original::digest(b"exact private response"),
            response_json_bytes: 300,
        }
    }

    #[test]
    fn text_rejections_bind_fresh_requests_and_keep_append_only_history_and_paid_bytes() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let (_, requests) = work.prepare_inspections(root.path(), &source, 2).unwrap();
        let request = &requests[0];
        let node = request["node_id"].as_str().unwrap();
        let contract = "a".repeat(64);
        let changed = "b".repeat(64);
        let plan = root
            .path()
            .join(work_path(&work.plan.id, "plan.json").unwrap());
        let original_plan = std::fs::read(&plan).unwrap();
        session
            .apply(
                work.plan_inspection_action(
                    root.path(),
                    &source,
                    request,
                    "legacy-contract",
                    "Explicit old action.".into(),
                )
                .unwrap(),
            )
            .unwrap();
        let action = root
            .path()
            .join(work_path(&work.plan.id, "inspection-actions.json").unwrap());
        let original_action = std::fs::read(&action).unwrap();
        let target = root.path().join(path(&work).unwrap());
        for kind in 0..6 {
            let mut rejected = reply(request);
            match kind {
                0 => rejected.output.work_id = "c".repeat(64),
                1 => rejected.output.input_revision = "c".repeat(64),
                2 => rejected.violation = ObservationTextError::FindingStatement { index: 0 },
                3 => rejected.response_json_bytes = 0,
                4 => rejected.response_json_bytes = work.plan.max_result_bytes + 1,
                5 => rejected.output.findings.push(WorkFinding {
                    statement: "Valid text with invalid evidence.".into(),
                    scope: None,
                    effective_time: None,
                    attribution: None,
                    evidence: vec![SemanticRange {
                        resource: 99,
                        start: 0,
                        end: 1,
                    }],
                }),
                _ => unreachable!(),
            }
            assert!(
                work.plan_observation_rejection(root.path(), &source, request, &contract, rejected)
                    .is_err(),
                "{kind}"
            );
            assert!(!target.exists());
        }
        let mut altered = request.clone();
        altered["unexpected"] = true.into();
        assert!(
            work.plan_observation_rejection(
                root.path(),
                &source,
                &altered,
                &contract,
                reply(request)
            )
            .is_err()
        );
        let mut changed_source = source.clone();
        changed_source[0].text.push_str("Changed source.");
        assert!(
            work.plan_observation_rejection(
                root.path(),
                &changed_source,
                request,
                &contract,
                reply(request)
            )
            .is_err()
        );
        let pending = root.path().join(".lorekeeper/mutation/pending.json");
        std::fs::create_dir_all(pending.parent().unwrap()).unwrap();
        std::fs::write(&pending, "{}").unwrap();
        assert!(
            work.plan_observation_rejection(
                root.path(),
                &source,
                request,
                &contract,
                reply(request)
            )
            .is_err()
        );
        assert!(!target.exists());
        std::fs::remove_file(pending).unwrap();
        let mutation = work
            .plan_observation_rejection(root.path(), &source, request, &contract, reply(request))
            .unwrap();
        std::fs::write(&target, "{}").unwrap();
        assert!(session.apply(mutation).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"{}");
        std::fs::remove_file(&target).unwrap();
        session
            .apply(
                work.plan_observation_rejection(
                    root.path(),
                    &source,
                    request,
                    &contract,
                    reply(request),
                )
                .unwrap(),
            )
            .unwrap();
        let first = std::fs::read_to_string(&target).unwrap();
        assert!(
            work.load_inspection_action(root.path(), request, &contract)
                .unwrap()
                .unwrap()
                .contains("summary")
        );
        assert!(
            work.load_inspection_action(root.path(), request, &changed)
                .unwrap()
                .is_none()
        );
        assert!(
            work.plan_observation_rejection(
                root.path(),
                &source,
                request,
                &contract,
                reply(request)
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), first);
        session
            .apply(
                work.plan_observation_rejection(
                    root.path(),
                    &source,
                    request,
                    &changed,
                    reply(request),
                )
                .unwrap(),
            )
            .unwrap();
        let old: Value = serde_json::from_str(&first).unwrap();
        let raw = std::fs::read_to_string(&target).unwrap();
        let new: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(new["entries"].as_object().unwrap().len(), 2);
        for (key, value) in old["entries"].as_object().unwrap() {
            assert_eq!(&new["entries"][key], value);
        }
        for corrupt in [
            raw.replace("\"schema\":", "\"schema\":null,\"schema\":"),
            raw.replace(
                "\"response_json_bytes\":",
                "\"response_json_bytes\":null,\"response_json_bytes\":",
            ),
            raw.replace("\"field\":\"summary\"", "\"field\":\"unknown\""),
            format!("{raw}{}", " ".repeat(work.max_input_bytes)),
        ] {
            assert_ne!(corrupt, raw);
            std::fs::write(&target, &corrupt).unwrap();
            assert!(work.progress(root.path(), &source).is_err());
            assert!(
                work.load_inspection_action(root.path(), request, &contract)
                    .is_err()
            );
        }
        std::fs::write(&target, &raw).unwrap();
        let mut good = reply(&requests[1]).output;
        good.summary = "Faithful independent peer.".into();
        work.record(&session, good, &source).unwrap();
        assert_eq!(work.progress(root.path(), &source).unwrap().completed, 1);
        assert!(
            !root
                .path()
                .join(output_path(&work.plan.id, node).unwrap())
                .exists()
        );
        assert_eq!(std::fs::read(&plan).unwrap(), original_plan);
        assert_eq!(std::fs::read(&action).unwrap(), original_action);
        assert_eq!(std::fs::read_to_string(target).unwrap(), raw);
    }

    #[test]
    fn interrupted_rejection_recovery_accepts_own_write_and_refuses_changed_dependencies() {
        for receipted in [false, true] {
            for drift in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let source = resources();
                let session = MutationSession::acquire(root.path()).unwrap();
                let work = Work::create(&session, &source, "subject", context("subject"), limits())
                    .unwrap();
                let (_, requests) = work.prepare_inspections(root.path(), &source, 1).unwrap();
                let request = &requests[0];
                let contract = "a".repeat(64);
                let plan = work
                    .plan_observation_rejection(
                        root.path(),
                        &source,
                        request,
                        &contract,
                        reply(request),
                    )
                    .unwrap();
                let plan_revision = lk_core::concept::file_revision(&encoded(&plan).unwrap());
                let value = serde_json::to_value(&plan).unwrap();
                let steps = value["steps"].as_array().unwrap();
                let edit = steps.last().unwrap();
                assert_eq!(edit["path"], path(&work).unwrap().to_str().unwrap());
                assert!(edit["before"].is_null());
                let after = edit["after"].as_str().unwrap().to_owned();
                let completed = steps.len() - usize::from(!receipted);
                let manifest = root
                    .path()
                    .join(work_path(&work.plan.id, "plan.json").unwrap());
                let original = std::fs::read(&manifest).unwrap();
                drop(session);
                let journal = root.path().join(".lorekeeper/mutation");
                let pending = encoded(&json!({"version":1,"plan":value})).unwrap();
                std::fs::write(journal.join("pending.json"), &pending).unwrap();
                std::fs::write(root.path().join(path(&work).unwrap()), &after).unwrap();
                std::fs::write(journal.join("progress.json"),encoded(&json!({
                    "plan_revision":plan_revision,"completed":completed,
                    "checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{completed}")),
                })).unwrap()).unwrap();
                if drift {
                    std::fs::write(&manifest, "{}").unwrap();
                    assert!(MutationSession::acquire(root.path()).is_err());
                    assert_eq!(
                        std::fs::read_to_string(journal.join("pending.json")).unwrap(),
                        pending
                    );
                    std::fs::write(&manifest, &original).unwrap();
                }
                let _recovered = MutationSession::acquire(root.path()).unwrap();
                assert!(!journal.join("pending.json").exists());
                assert_eq!(
                    std::fs::read_to_string(root.path().join(path(&work).unwrap())).unwrap(),
                    after
                );
                assert!(
                    work.load_inspection_action(root.path(), request, &contract)
                        .unwrap()
                        .is_some()
                );
                assert!(!work.progress(root.path(), &source).unwrap().complete);
                assert_eq!(std::fs::read(manifest).unwrap(), original);
            }
        }
    }
}
