use super::*;

pub(crate) struct ObservationCandidate {
    pub output: WorkOutput,
    pub text_violation: Option<ObservationTextError>,
    pub response_revision: String,
    pub response_json_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum ObservationViolation {
    Text(ObservationTextError),
    Evidence(ObservationEvidenceError),
}

impl std::fmt::Display for ObservationViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(error) => error.fmt(formatter),
            Self::Evidence(error) => error.fmt(formatter),
        }
    }
}

pub(crate) enum ObservationRecordPlan {
    Accepted(MutationPlan),
    Rejected {
        mutation: MutationPlan,
        violation: ObservationViolation,
    },
}

pub(crate) struct ObservationRejections {
    work_id: String,
    entries: BTreeMap<String, Rejection>,
    version_two: bool,
    legacy: Option<LegacySnapshot>,
    historical: BTreeMap<String, Rejection>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "schema", deny_unknown_fields)]
enum Record {
    #[serde(rename = "observation-rejections-v1")]
    Legacy {
        work_id: String,
        #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
        entries: BTreeMap<String, TextRejection>,
    },
    #[serde(rename = "observation-rejections-v2")]
    Current {
        work_id: String,
        #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
        entries: BTreeMap<String, Rejection>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy: Option<LegacySnapshot>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySnapshot {
    revision: String,
    raw: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextRejection {
    node_id: String,
    input_revision: String,
    contract_revision: String,
    response_revision: String,
    response_json_bytes: usize,
    violation: ObservationTextError,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rejection {
    node_id: String,
    input_revision: String,
    contract_revision: String,
    response_revision: String,
    response_json_bytes: usize,
    violation: ObservationViolation,
}

impl From<TextRejection> for Rejection {
    fn from(value: TextRejection) -> Self {
        Self {
            node_id: value.node_id,
            input_revision: value.input_revision,
            contract_revision: value.contract_revision,
            response_revision: value.response_revision,
            response_json_bytes: value.response_json_bytes,
            violation: ObservationViolation::Text(value.violation),
        }
    }
}

impl Serialize for ObservationRejections {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let record = if self.version_two {
            Record::Current {
                work_id: self.work_id.clone(),
                entries: self.entries.clone(),
                legacy: self.legacy.clone(),
            }
        } else {
            let entries = self
                .entries
                .iter()
                .map(|(key, entry)| {
                    let ObservationViolation::Text(violation) = &entry.violation else {
                        return Err(serde::ser::Error::custom(
                            "legacy rejection cannot contain an evidence violation",
                        ));
                    };
                    Ok((
                        key.clone(),
                        TextRejection {
                            node_id: entry.node_id.clone(),
                            input_revision: entry.input_revision.clone(),
                            contract_revision: entry.contract_revision.clone(),
                            response_revision: entry.response_revision.clone(),
                            response_json_bytes: entry.response_json_bytes,
                            violation: violation.clone(),
                        },
                    ))
                })
                .collect::<Result<_, S::Error>>()?;
            Record::Legacy {
                work_id: self.work_id.clone(),
                entries,
            }
        };
        record.serialize(serializer)
    }
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
        let record: Record = crate::commands::work::receipt::decode(raw).map_err(fail)?;
        let (work_id, entries, version_two, legacy) = match record {
            Record::Legacy { work_id, entries } => (
                work_id,
                entries
                    .into_iter()
                    .map(|(key, entry)| (key, entry.into()))
                    .collect(),
                false,
                None,
            ),
            Record::Current {
                work_id,
                entries,
                legacy,
            } => (work_id, entries, true, legacy),
        };
        if work_id != work.plan.id || entries.is_empty() {
            return Err(fail("invalid observation rejection identity or entries"));
        }
        let mut historical = BTreeMap::new();
        if let Some(snapshot) = &legacy {
            require_id(&snapshot.revision)?;
            if lk_core::rss_original::digest(snapshot.raw.as_bytes()) != snapshot.revision {
                return Err(fail(
                    "legacy observation rejection bytes differ from their digest",
                ));
            }
            let prior: Record =
                crate::commands::work::receipt::decode(&snapshot.raw).map_err(fail)?;
            if !matches!(prior, Record::Legacy { .. }) {
                return Err(fail("observation rejection snapshot requires legacy v1"));
            }
            historical = Self::decode(&snapshot.raw, work, nodes)?.entries;
        }
        for (request, entry) in entries.iter().chain(&historical) {
            for id in [
                request,
                &entry.node_id,
                &entry.input_revision,
                &entry.contract_revision,
                &entry.response_revision,
            ] {
                require_id(id)?;
            }
            let node = nodes
                .iter()
                .find(|node| node.id == entry.node_id)
                .ok_or_else(|| fail("observation rejection names an unknown node"))?;
            if entry.response_json_bytes == 0
                || entry.response_json_bytes > work.plan.max_result_bytes
            {
                return Err(fail("invalid observation rejection response bound"));
            }
            let bound = work
                .max_input_bytes
                .saturating_add(entry.response_json_bytes);
            match &entry.violation {
                ObservationViolation::Text(error) => {
                    if error.index().is_some_and(|index| index >= bound) {
                        return Err(fail("invalid observation rejection field index"));
                    }
                }
                ObservationViolation::Evidence(error) => error.validate(node, bound)?,
            }
        }
        if historical.keys().any(|key| entries.contains_key(key)) {
            return Err(fail(
                "observation rejection duplicates a historical request",
            ));
        }
        Ok(Self {
            work_id,
            entries,
            version_two,
            legacy,
            historical,
        })
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
        self.entries
            .values()
            .chain(self.historical.values())
            .any(|entry| entry.node_id == node)
    }

    pub(crate) fn reason(
        &self,
        work: &Work,
        request: &Value,
        contract: &str,
    ) -> miette::Result<Option<String>> {
        let key = request_revision(request, contract)?;
        let Some(entry) = self.entries.get(&key).or_else(|| self.historical.get(&key)) else {
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
    pub fn plan_native_observation(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        candidate: ObservationCandidate,
    ) -> miette::Result<ObservationRecordPlan> {
        require_id(contract)?;
        require_id(&candidate.response_revision)?;
        if candidate.response_json_bytes == 0
            || candidate.response_json_bytes > self.plan.max_result_bytes
        {
            return Err(fail("invalid observation response bound"));
        }
        let evaluated = self.evaluate(root, resources)?;
        let node = self.validate_output_node(&candidate.output, &evaluated)?;
        if !evaluated.ready.contains(&node.id) {
            return Err(fail("native observation requires an unfinished ready node"));
        }
        if self
            .node_input(node, &evaluated.inputs, &evaluated.current)
            .map_err(NodeInputError::report)?
            != *request
        {
            return Err(fail("observation request changed before recording"));
        }
        let children = match &node.kind {
            NodeKind::Inspect { .. } => Vec::new(),
            NodeKind::Reduce { children } => children
                .iter()
                .map(|id| &evaluated.current[id].output)
                .collect(),
        };
        validate_output_questions(&candidate.output, &children)?;
        let text_violation = candidate.output.validate_text().err();
        if text_violation != candidate.text_violation {
            return Err(fail(
                "observation text disposition differs from the decoded response",
            ));
        }
        let violation = validate_output_evidence(&candidate.output, node)
            .err()
            .map(ObservationViolation::Evidence)
            .or_else(|| text_violation.map(ObservationViolation::Text));
        let Some(violation) = violation else {
            return self
                .plan_evaluated_record(root, candidate.output, evaluated, None, Vec::new())
                .map(ObservationRecordPlan::Accepted);
        };
        let node_id = node.id.clone();
        let target = path(self)?;
        let before = evaluated.raw.get(&target).cloned();
        let mut records = match &before {
            Some(raw) => ObservationRejections::decode(raw, self, &evaluated.nodes)?,
            None => ObservationRejections {
                work_id: self.plan.id.clone(),
                entries: BTreeMap::new(),
                version_two: false,
                legacy: None,
                historical: BTreeMap::new(),
            },
        };
        let key = request_revision(request, contract)?;
        if records.entries.contains_key(&key) || records.historical.contains_key(&key) {
            return Err(fail(
                "observation rejection already retained for this exact request",
            ));
        }
        if matches!(violation, ObservationViolation::Evidence(_)) && !records.version_two {
            records.version_two = true;
            if let Some(raw) = &before {
                records.legacy = Some(LegacySnapshot {
                    revision: lk_core::rss_original::digest(raw.as_bytes()),
                    raw: raw.clone(),
                });
                records.historical = std::mem::take(&mut records.entries);
            }
        }
        records.entries.insert(
            key,
            Rejection {
                node_id: node_id.clone(),
                input_revision: candidate.output.input_revision,
                contract_revision: contract.into(),
                response_revision: candidate.response_revision,
                response_json_bytes: candidate.response_json_bytes,
                violation: violation.clone(),
            },
        );
        let after = encoded(&records)?;
        ObservationRejections::decode(&after, self, &evaluated.nodes)?;
        let mut mutation = MutationPlan::default();
        if evaluated.refinements.is_none() {
            mutation.require_absent(super::super::refinement::path(self)?);
        }
        let output = output_path(&self.plan.id, &node_id)?;
        if !evaluated.raw.contains_key(&output) {
            mutation.require_absent(output);
        }
        for (path, raw) in evaluated.raw {
            if path != target {
                mutation.require_unchanged(path, raw);
            }
        }
        mutation
            .publish_expected(root, target, before, after)
            .map_err(fail)?;
        Ok(ObservationRecordPlan::Rejected {
            mutation,
            violation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::{context, limits, resources};
    use super::*;

    impl Work {
        fn rejection_plan(
            &self,
            root: &Path,
            resources: &[SemanticResource],
            request: &Value,
            contract: &str,
            candidate: ObservationCandidate,
        ) -> miette::Result<MutationPlan> {
            match self.plan_native_observation(root, resources, request, contract, candidate)? {
                ObservationRecordPlan::Rejected { mutation, .. } => Ok(mutation),
                ObservationRecordPlan::Accepted(_) => {
                    Err(fail("test expected an authored rejection"))
                }
            }
        }
    }

    fn reply(request: &Value) -> ObservationCandidate {
        ObservationCandidate {
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
            text_violation: Some(ObservationTextError::Summary),
            response_revision: lk_core::rss_original::digest(b"exact private response"),
            response_json_bytes: 300,
        }
    }

    #[test]
    fn text_rejections_bind_fresh_requests_and_keep_append_only_history_and_paid_bytes() {
        for policy in [None, Some(lk_core::semantic::INSPECTION_CHARACTERS)] {
            check_text_rejection_history(policy);
        }
    }

    fn check_text_rejection_history(policy: Option<std::num::NonZeroUsize>) {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::build_with_inspection(&source, "subject", context("subject"), limits(), policy)
                .unwrap()
                .persist(&session)
                .unwrap();
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
                2 => rejected.output.node_id = "c".repeat(64),
                3 => rejected.response_json_bytes = 0,
                4 => rejected.response_json_bytes = work.plan.max_result_bytes + 1,
                5 => rejected.output.resolutions.push(WorkResolution {
                    id: "unknown".into(),
                    reason: "Unknown inherited concern with invalid evidence.".into(),
                    evidence: vec![SemanticRange {
                        resource: 99,
                        start: 0,
                        end: 1,
                    }],
                }),
                _ => unreachable!(),
            }
            assert!(
                work.rejection_plan(root.path(), &source, request, &contract, rejected)
                    .is_err(),
                "{kind}"
            );
            assert!(!target.exists());
        }
        let mut altered = request.clone();
        altered["unexpected"] = true.into();
        assert!(
            work.rejection_plan(root.path(), &source, &altered, &contract, reply(request))
                .is_err()
        );
        let mut changed_source = source.clone();
        changed_source[0].text.push_str("Changed source.");
        assert!(
            work.rejection_plan(
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
            work.rejection_plan(root.path(), &source, request, &contract, reply(request))
                .is_err()
        );
        assert!(!target.exists());
        std::fs::remove_file(pending).unwrap();
        let mutation = work
            .rejection_plan(root.path(), &source, request, &contract, reply(request))
            .unwrap();
        std::fs::write(&target, "{}").unwrap();
        assert!(session.apply(mutation).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"{}");
        std::fs::remove_file(&target).unwrap();
        session
            .apply(
                work.rejection_plan(root.path(), &source, request, &contract, reply(request))
                    .unwrap(),
            )
            .unwrap();
        let first = std::fs::read_to_string(&target).unwrap();
        assert!(
            work.load_inspection_hold(root.path(), request, &contract)
                .unwrap()
                .unwrap()
                .contains("summary")
        );
        let changed_hold = work
            .load_inspection_hold(root.path(), request, &changed)
            .unwrap();
        if policy.is_none() {
            assert!(changed_hold.unwrap().contains("Explicit old action."));
        } else {
            assert!(changed_hold.is_none());
        }
        assert!(
            work.rejection_plan(root.path(), &source, request, &contract, reply(request))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), first);
        session
            .apply(
                work.rejection_plan(root.path(), &source, request, &changed, reply(request))
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
                work.load_inspection_hold(root.path(), request, &contract)
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
                    .rejection_plan(root.path(), &source, request, &contract, reply(request))
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
                    work.load_inspection_hold(root.path(), request, &contract)
                        .unwrap()
                        .is_some()
                );
                assert!(!work.progress(root.path(), &source).unwrap().complete);
                assert_eq!(std::fs::read(manifest).unwrap(), original);
            }
        }
    }

    fn evidence_reply(request: &Value) -> ObservationCandidate {
        let mut candidate = reply(request);
        candidate.output.summary = "Read assigned evidence without altering it.".into();
        candidate.text_violation = None;
        candidate.output.findings.push(WorkFinding {
            statement: "Unsupported new reference.".into(),
            scope: None,
            effective_time: None,
            attribution: None,
            evidence: vec![SemanticRange {
                resource: 99,
                start: 0,
                end: 1,
            }],
        });
        candidate
    }

    #[test]
    fn evidence_history_retains_exact_legacy_bytes_and_matches_both_contracts() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let (_, requests) = work.prepare_inspections(root.path(), &source, 2).unwrap();
        let mut peer = reply(&requests[1]).output;
        peer.summary = "Complete independent inspection.".into();
        work.record(&session, peer, &source).unwrap();
        let peer_path =
            output_path(&work.plan.id, requests[1]["node_id"].as_str().unwrap()).unwrap();
        let peer_before = std::fs::read(root.path().join(&peer_path)).unwrap();
        let request = &requests[0];
        let old_contract = "a".repeat(64);
        let contract = "b".repeat(64);
        session
            .apply(
                work.rejection_plan(root.path(), &source, request, &old_contract, reply(request))
                    .unwrap(),
            )
            .unwrap();
        let target = root.path().join(path(&work).unwrap());
        let legacy = std::fs::read_to_string(&target).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&legacy).unwrap()["schema"],
            "observation-rejections-v1"
        );
        session
            .apply(
                work.rejection_plan(
                    root.path(),
                    &source,
                    request,
                    &contract,
                    evidence_reply(request),
                )
                .unwrap(),
            )
            .unwrap();
        let raw = std::fs::read_to_string(&target).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["schema"], "observation-rejections-v2");
        assert_eq!(value["legacy"]["raw"], legacy);
        assert_eq!(
            value["legacy"]["revision"],
            lk_core::rss_original::digest(legacy.as_bytes())
        );
        for expected in [&old_contract, &contract] {
            assert!(
                work.load_inspection_hold(root.path(), request, expected)
                    .unwrap()
                    .is_some()
            );
        }
        assert!(
            work.load_inspection_hold(root.path(), request, &"c".repeat(64))
                .unwrap()
                .is_none()
        );
        assert!(
            work.rejection_plan(
                root.path(),
                &source,
                request,
                &contract,
                evidence_reply(request)
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), raw);
        assert_eq!(
            std::fs::read(root.path().join(&peer_path)).unwrap(),
            peer_before
        );
        assert!(
            !root
                .path()
                .join(output_path(&work.plan.id, request["node_id"].as_str().unwrap()).unwrap())
                .exists()
        );
        let nodes = work.nodes().unwrap();
        let records = ObservationRejections::decode(&raw, &work, &nodes).unwrap();
        assert_eq!(records.entries.len(), 1);
        assert_eq!(records.historical.len(), 1);
        let mut corrupted = value.clone();
        corrupted["legacy"]["raw"] = format!("{legacy} ").into();
        assert!(ObservationRejections::decode(&corrupted.to_string(), &work, &nodes).is_err());
        let key = value["entries"].as_object().unwrap().keys().next().unwrap();
        corrupted = value.clone();
        corrupted["entries"][key]["violation"]["reference"]["resource"] = 0.into();
        assert!(ObservationRejections::decode(&corrupted.to_string(), &work, &nodes).is_err());
        corrupted = value.clone();
        let historical_key = records.historical.keys().next().unwrap();
        corrupted["entries"]
            .as_object_mut()
            .unwrap()
            .insert(historical_key.clone(), value["entries"][key].clone());
        assert!(ObservationRejections::decode(&corrupted.to_string(), &work, &nodes).is_err());
        let mut bounded = work.clone();
        bounded.max_input_bytes = raw.len();
        ObservationRejections::decode(&raw, &bounded, &nodes).unwrap();
        bounded.max_input_bytes -= 1;
        assert!(ObservationRejections::decode(&raw, &bounded, &nodes).is_err());
        let mut at_limit = value.clone();
        at_limit["entries"][key]["response_json_bytes"] = work.plan.max_result_bytes.into();
        ObservationRejections::decode(&at_limit.to_string(), &work, &nodes).unwrap();
        at_limit["entries"][key]["response_json_bytes"] = (work.plan.max_result_bytes + 1).into();
        assert!(ObservationRejections::decode(&at_limit.to_string(), &work, &nodes).is_err());
    }

    #[test]
    fn a_corrupt_stored_peer_cannot_be_hidden_by_an_authored_rejection() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let (_, requests) = work.prepare_inspections(root.path(), &source, 2).unwrap();
        let mut peer = reply(&requests[1]).output;
        peer.summary = "Complete independent inspection.".into();
        work.record(&session, peer, &source).unwrap();
        let path = root
            .path()
            .join(output_path(&work.plan.id, requests[1]["node_id"].as_str().unwrap()).unwrap());
        let mut stored: StoredOutput =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        stored.output.findings = evidence_reply(&requests[1]).output.findings;
        stored.output_revision = revision(&stored.output).unwrap();
        let corrupted = encoded(&stored).unwrap();
        std::fs::write(&path, &corrupted).unwrap();
        let error = work
            .plan_native_observation(
                root.path(),
                &source,
                &requests[0],
                &"a".repeat(64),
                evidence_reply(&requests[0]),
            )
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("outside the node's assigned ranges")
        );
        assert!(!root.path().join(super::path(&work).unwrap()).exists());
        assert_eq!(std::fs::read_to_string(path).unwrap(), corrupted);
    }

    #[test]
    fn intermediate_reduction_checks_inheritance_before_local_evidence_and_keeps_paid_nodes() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut source = resources();
        source[0].text = "source paragraph ".repeat(3000);
        let work = Work::create(
            &session,
            &source,
            "subject",
            context("subject"),
            WorkLimits {
                max_input_bytes: 8000,
                max_result_bytes: 1500,
            },
        )
        .unwrap();
        let nodes = work.nodes().unwrap();
        let mut original = BTreeMap::new();
        let plan_path = work_path(&work.plan.id, "plan.json").unwrap();
        original.insert(
            plan_path.clone(),
            std::fs::read(root.path().join(&plan_path)).unwrap(),
        );
        for (index, part) in work.plan.parts.iter().enumerate() {
            let request = work.input(root.path(), &part.id, &source).unwrap();
            let mut output = reply(&request).output;
            output.summary = "Read exactly the assigned source.".into();
            if index == 0 {
                output.source_questions.push(WorkQuestion {
                    id: "source:question".into(),
                    question: "What remains unknown?".into(),
                    evidence: vec![
                        nodes.iter().find(|node| node.id == part.id).unwrap().ranges[0].clone(),
                    ],
                });
            }
            work.record(&session, output, &source).unwrap();
            let path = output_path(&work.plan.id, &part.id).unwrap();
            original.insert(path.clone(), std::fs::read(root.path().join(path)).unwrap());
        }
        let (_, request) = work.prepare_inspection(root.path(), &source).unwrap();
        let request = request.unwrap();
        assert_eq!(request["kind"], "reduce");
        assert_ne!(request["node_id"], nodes.last().unwrap().id);
        let mut candidate = evidence_reply(&request);
        candidate.output.source_questions = request["children"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|child| {
                serde_json::from_value::<WorkOutput>(child["output"].clone())
                    .unwrap()
                    .source_questions
            })
            .collect();
        assert_eq!(candidate.output.source_questions.len(), 1);
        let mut changed = evidence_reply(&request);
        changed.output.source_questions = candidate.output.source_questions.clone();
        changed.output.source_questions[0].question = "Changed inherited text.".into();
        assert!(
            work.plan_native_observation(root.path(), &source, &request, &"a".repeat(64), changed)
                .is_err()
        );
        assert!(!root.path().join(path(&work).unwrap()).exists());
        let ObservationRecordPlan::Rejected {
            mutation,
            violation: ObservationViolation::Evidence(_),
        } = work
            .plan_native_observation(root.path(), &source, &request, &"a".repeat(64), candidate)
            .unwrap()
        else {
            panic!("expected a new authored evidence rejection")
        };
        session.apply(mutation).unwrap();
        assert_eq!(
            work.input(root.path(), request["node_id"].as_str().unwrap(), &source)
                .unwrap(),
            request
        );
        assert!(
            work.load_inspection_hold(root.path(), &request, &"a".repeat(64))
                .unwrap()
                .is_some()
        );
        for (path, bytes) in original {
            assert_eq!(std::fs::read(root.path().join(path)).unwrap(), bytes);
        }
    }
}
