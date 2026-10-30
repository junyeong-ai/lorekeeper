use super::*;
use inspection::{InspectionAction, InspectionActions, request_revision};
use lk_core::semantic::SemanticPart;

const SCHEMA: &str = "inspection-refinements-v1";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Refinements {
    schema: String,
    work_id: String,
    legacy_revision: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    entries: BTreeMap<String, Refinement>,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    actions: BTreeMap<String, ActionRecord>,
}

pub(super) struct CapturedRefinements {
    pub nodes: Vec<Node>,
    pub state: Option<Refinements>,
    pub files: BTreeMap<PathBuf, String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Refinement {
    input_revision: String,
    request_revision: String,
    max_part_characters: usize,
    parts: Vec<SemanticPart>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRecord {
    node_id: String,
    action: InspectionAction,
}

#[derive(Serialize)]
#[cfg(unix)]
pub(in crate::commands::work) struct RefinementPreview {
    pub work_id: String,
    pub node_id: String,
    pub max_part_characters: usize,
    pub inspection_nodes: Vec<String>,
    pub reduction_nodes: usize,
    pub already_refined: bool,
}

pub(super) fn path(work: &Work) -> miette::Result<PathBuf> {
    work_path(&work.plan.id, "inspection-refinements.json")
}

fn characters(ranges: &[SemanticRange]) -> miette::Result<usize> {
    ranges.iter().try_fold(0usize, |count, range| {
        range
            .end
            .checked_sub(range.start)
            .and_then(|length| count.checked_add(length))
            .ok_or_else(|| fail("invalid refinement character count"))
    })
}

fn verify_coverage(assigned: &[SemanticRange], parts: &[SemanticPart]) -> miette::Result<()> {
    let mut index = 0;
    let mut cursor = assigned
        .first()
        .ok_or_else(|| fail("refinement has no assigned evidence"))?
        .start;
    for range in parts.iter().flat_map(|part| &part.ranges) {
        let expected = assigned
            .get(index)
            .ok_or_else(|| fail("refinement adds unassigned evidence"))?;
        if range.resource != expected.resource
            || range.start != cursor
            || range.end < range.start
            || range.end > expected.end
            || (range.start == range.end && expected.start != expected.end)
        {
            return Err(fail(
                "refinement coverage is missing, overlapping or outside its assignment",
            ));
        }
        cursor = range.end;
        if cursor == expected.end {
            index += 1;
            cursor = assigned.get(index).map_or(0, |range| range.start);
        }
    }
    if index != assigned.len() {
        return Err(fail("refinement does not cover the complete assignment"));
    }
    Ok(())
}

impl Refinement {
    fn part_id(
        &self,
        work: &Work,
        node: &str,
        index: usize,
        ranges: &[SemanticRange],
    ) -> miette::Result<String> {
        revision(&json!([
            SCHEMA,
            work.plan.id,
            node,
            self.input_revision,
            self.request_revision,
            self.max_part_characters,
            index,
            ranges
        ]))
    }

    fn tree(&self, work: &Work, parent: &Node) -> miette::Result<Vec<Node>> {
        require_id(&self.input_revision)?;
        require_id(&self.request_revision)?;
        if !matches!(parent.kind, NodeKind::Inspect { .. })
            || self.parts.len() < 2
            || self.max_part_characters == 0
            || self.max_part_characters >= characters(&parent.ranges)?
        {
            return Err(fail(
                "refinement requires a strictly smaller inspection assignment",
            ));
        }
        verify_coverage(&parent.ranges, &self.parts)?;
        let provenance = revision(self)?;
        let mut leaves = Vec::new();
        for (index, part) in self.parts.iter().enumerate() {
            if part.ranges.is_empty()
                || characters(&part.ranges)? == 0
                || characters(&part.ranges)? > self.max_part_characters
                || part.id != self.part_id(work, &parent.id, index, &part.ranges)?
            {
                return Err(fail(
                    "invalid refinement part identity, character bound or assignment",
                ));
            }
            leaves.push(Node {
                id: part.id.clone(),
                kind: NodeKind::Inspect {
                    part_id: part.id.clone(),
                },
                ranges: part.ranges.clone(),
                refinement: Some(provenance.clone()),
            });
        }
        if work.reserved_reduce_bytes(&leaves[..2])? > work.max_input_bytes {
            return Err(fail(
                "refinement cannot reserve a two-child reduction within the input bound",
            ));
        }
        let mut nodes = work.reduce_nodes(
            leaves,
            work.max_input_bytes,
            Some(&provenance),
            |children| {
                revision(&json!([
                    SCHEMA,
                    "reduce",
                    work.plan.id,
                    provenance,
                    children
                ]))
            },
        )?;
        let root = nodes.last_mut().expect("refinement has a reduction");
        root.id.clone_from(&parent.id);
        root.ranges.clone_from(&parent.ranges);
        Ok(nodes)
    }
}

impl Refinements {
    pub(super) fn has_action_for_node(&self, node: &str) -> bool {
        self.actions.values().any(|action| action.node_id == node)
    }
    fn encode(&self, work: &Work) -> miette::Result<String> {
        let raw = encoded(self)?;
        if raw.len() > work.max_input_bytes {
            return Err(InputCapacity {
                actual_bytes: raw.len(),
                max_bytes: work.max_input_bytes,
            }
            .into());
        }
        Ok(raw)
    }

    fn expand(&self, work: &Work) -> miette::Result<Vec<Node>> {
        if self.schema != SCHEMA || self.work_id != work.plan.id || self.entries.is_empty() {
            return Err(fail("invalid inspection refinement identity or schema"));
        }
        require_id(&self.legacy_revision)?;
        let subject: super::super::resources::Subject =
            serde_json::from_value(work.context["subject"].clone()).map_err(fail)?;
        if !matches!(subject, super::super::resources::Subject::Audit { .. }) {
            return Err(fail("inspection refinement requires audit work"));
        }
        let mut nodes = work.nodes()?;
        let mut expanded = BTreeSet::new();
        let mut at = 0;
        while at < nodes.len() {
            let node = &nodes[at];
            if let Some(entry) = self.entries.get(&node.id)
                && !expanded.contains(&node.id)
            {
                let action = self
                    .actions
                    .get(&entry.request_revision)
                    .ok_or_else(|| fail("refinement originating action is missing"))?;
                if action.node_id != node.id || action.action.input_revision != entry.input_revision
                {
                    return Err(fail(
                        "refinement originating action differs from its assignment",
                    ));
                }
                let tree = entry.tree(work, node)?;
                expanded.insert(node.id.clone());
                nodes.splice(at..=at, tree);
            } else {
                at += 1;
            }
        }
        let known: BTreeSet<_> = nodes.iter().map(|node| node.id.as_str()).collect();
        if known.len() != nodes.len() || expanded.len() != self.entries.len() {
            return Err(fail(
                "refinement contains duplicate, unreachable or cyclic nodes",
            ));
        }
        for (key, action) in &self.actions {
            action.action.validate(work)?;
            if key != &action.action.request_revision || !known.contains(action.node_id.as_str()) {
                return Err(fail("refinement action names an unknown request or node"));
            }
        }
        Ok(nodes)
    }

    pub(super) fn validate_inputs(
        &self,
        work: &Work,
        inputs: &SemanticInputs<'_>,
        nodes: &[Node],
    ) -> miette::Result<()> {
        for (id, entry) in &self.entries {
            let node = nodes
                .iter()
                .find(|node| &node.id == id)
                .expect("expanded refinement node");
            let part = inputs.input_for_ranges(id, &node.ranges).map_err(fail)?;
            let request = work
                .inspect_input(id, part)
                .map_err(NodeInputError::report)?;
            if request["input_revision"].as_str() != Some(entry.input_revision.as_str()) {
                return Err(fail("refinement source assignment or context changed"));
            }
        }
        Ok(())
    }

    pub(super) fn action(
        &self,
        work: &Work,
        request: &Value,
        contract: &str,
    ) -> miette::Result<Option<InspectionAction>> {
        let key = request_revision(request, contract)?;
        let Some(record) = self.actions.get(&key) else {
            return Ok(None);
        };
        if request["work_id"] != work.plan.id
            || request["node_id"].as_str() != Some(record.node_id.as_str())
            || request["input_revision"].as_str() != Some(record.action.input_revision.as_str())
        {
            return Err(fail("refinement action identity differs from its request"));
        }
        Ok(Some(record.action.clone()))
    }

    pub(super) fn available_slots(&self, work: &Work) -> miette::Result<usize> {
        let placeholder = "0".repeat(64);
        let reserve = encoded(&json!({placeholder.clone(): ActionRecord {
            node_id:placeholder.clone(), action:InspectionAction {
                input_revision:placeholder.clone(), request_revision:placeholder,
                reason:String::new(),
                capacity:Some(inspection::OutputCapacity { actual_bytes:usize::MAX,
                    max_bytes:work.plan.max_result_bytes }),
            }
        }}))?
        .len()
        .checked_add(work.plan.max_result_bytes)
        .ok_or_else(|| fail("refinement action reservation overflow"))?;
        let bytes = self.encode(work)?.len();
        let slots = work.max_input_bytes.saturating_sub(bytes) / reserve;
        if slots == 0 {
            return Err(InputCapacity {
                actual_bytes: bytes.saturating_add(reserve),
                max_bytes: work.max_input_bytes,
            }
            .into());
        }
        Ok(slots)
    }

    pub(super) fn plan_action(
        mut self,
        work: &Work,
        root: &Path,
        request: &Value,
        action: InspectionAction,
        evaluated: Evaluation<'_>,
    ) -> miette::Result<MutationPlan> {
        action.validate(work)?;
        let record = ActionRecord {
            node_id: request["node_id"]
                .as_str()
                .ok_or_else(|| fail("inspection request has no node identity"))?
                .into(),
            action,
        };
        if let Some(prior) = self.actions.get(&record.action.request_revision)
            && prior != &record
        {
            return Err(fail(
                "refinement action cannot replace a different retained response",
            ));
        }
        self.actions
            .insert(record.action.request_revision.clone(), record);
        let after = self.encode(work)?;
        let target = path(work)?;
        let mut mutation = MutationPlan::default();
        let before = evaluated.raw.get(&target).cloned();
        let output = output_path(
            &work.plan.id,
            request["node_id"].as_str().expect("validated node"),
        )?;
        if !evaluated.raw.contains_key(&output) {
            mutation.require_absent(output);
        }
        for (dependency, raw) in evaluated.raw {
            if dependency != target {
                mutation.require_unchanged(dependency, raw);
            }
        }
        mutation
            .publish_expected(root, target, before, after)
            .map_err(fail)?;
        Ok(mutation)
    }
}

impl Work {
    pub(super) fn capture_refinements(&self, root: &Path) -> miette::Result<CapturedRefinements> {
        let raw = read_optional(root, &path(self)?)?;
        match raw {
            None => Ok(CapturedRefinements {
                nodes: self.nodes()?,
                state: None,
                files: BTreeMap::new(),
            }),
            Some(raw) => {
                if raw.len() > self.max_input_bytes {
                    return Err(fail("stored refinement exceeds its input bound"));
                }
                let state: Refinements = super::super::receipt::decode(&raw).map_err(fail)?;
                let nodes = state.expand(self)?;
                let legacy_path = work_path(&self.plan.id, "inspection-actions.json")?;
                let legacy_raw = read_optional(root, &legacy_path)?
                    .ok_or_else(|| fail("refinement original actions are missing"))?;
                InspectionActions::decode(&legacy_raw, self)?;
                if lk_core::concept::file_revision(&legacy_raw) != state.legacy_revision {
                    return Err(fail("refinement original actions changed"));
                }
                Ok(CapturedRefinements {
                    nodes,
                    state: Some(state),
                    files: BTreeMap::from([(path(self)?, raw), (legacy_path, legacy_raw)]),
                })
            }
        }
    }

    #[cfg(unix)]
    pub(in crate::commands::work) fn plan_refinement(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        node_id: &str,
        max_part_characters: usize,
        contract: impl Fn(&Value) -> miette::Result<String>,
    ) -> miette::Result<(MutationPlan, RefinementPreview)> {
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or_else(|| fail("unknown inspection node"))?;
        let legacy_path = work_path(&self.plan.id, "inspection-actions.json")?;
        let legacy_raw = match evaluated.raw.get(&legacy_path) {
            Some(raw) => Some(raw.clone()),
            None => read_optional(root, &legacy_path)?,
        };
        let mut state = match evaluated.refinements.clone() {
            Some(state) => state,
            None => Refinements {
                schema: SCHEMA.into(),
                work_id: self.plan.id.clone(),
                legacy_revision: lk_core::concept::file_revision(
                    legacy_raw.as_deref().ok_or_else(|| {
                        fail("refinement requires an exact current native processing action")
                    })?,
                ),
                entries: BTreeMap::new(),
                actions: BTreeMap::new(),
            },
        };
        let mut mutation = MutationPlan::default();
        let already_refined = state.entries.contains_key(node_id);
        let parent_output = output_path(&self.plan.id, node_id)?;
        if !already_refined && !evaluated.raw.contains_key(&parent_output) {
            mutation.require_absent(parent_output);
        }
        if !already_refined {
            if !matches!(node.kind, NodeKind::Inspect { .. }) || !evaluated.ready.contains(&node.id)
            {
                return Err(fail("refinement requires an unfinished ready inspection"));
            }
            let request = self
                .node_input(node, &evaluated.inputs, &evaluated.current)
                .map_err(NodeInputError::report)?;
            let contract = contract(&request)?;
            let action = match state.action(self, &request, &contract)? {
                Some(action) => Some(action),
                None => legacy_raw
                    .as_ref()
                    .map(|raw| InspectionActions::decode(raw, self))
                    .transpose()?
                    .map(|actions| actions.matching_action(self, &request, &contract))
                    .transpose()?
                    .flatten(),
            }
            .ok_or_else(|| fail("refinement requires an exact current native processing action"))?;
            if max_part_characters == 0 || max_part_characters >= characters(&node.ranges)? {
                return Err(fail(
                    "refinement requires a positive, strictly smaller character bound",
                ));
            }
            let groups = evaluated
                .inputs
                .refine_ranges(&node.ranges, max_part_characters, self.max_input_bytes / 64)
                .map_err(fail)?;
            let mut entry = Refinement {
                input_revision: action.input_revision.clone(),
                request_revision: action.request_revision.clone(),
                max_part_characters,
                parts: Vec::new(),
            };
            for (index, ranges) in groups.into_iter().enumerate() {
                let id = entry.part_id(self, node_id, index, &ranges)?;
                entry.parts.push(SemanticPart { id, ranges });
            }
            entry.tree(self, node)?;
            state
                .actions
                .entry(action.request_revision.clone())
                .or_insert(ActionRecord {
                    node_id: node_id.into(),
                    action,
                });
            state.entries.insert(node_id.into(), entry);
        }
        let entry = &state.entries[node_id];
        if entry.max_part_characters != max_part_characters {
            return Err(fail(
                "existing refinement is immutable; refine an unfinished child instead",
            ));
        }
        let nodes = state.expand(self)?;
        state.validate_inputs(self, &evaluated.inputs, &nodes)?;
        if !already_refined {
            state.available_slots(self)?;
        }
        let provenance = revision(entry)?;
        let preview = RefinementPreview {
            work_id: self.plan.id.clone(),
            node_id: node_id.into(),
            max_part_characters,
            inspection_nodes: entry.parts.iter().map(|part| part.id.clone()).collect(),
            reduction_nodes: nodes
                .iter()
                .filter(|node| {
                    node.refinement.as_ref() == Some(&provenance)
                        && matches!(node.kind, NodeKind::Reduce { .. })
                })
                .count(),
            already_refined,
        };
        let target = path(self)?;
        let before = evaluated.raw.get(&target).cloned();
        for (dependency, raw) in evaluated.raw {
            if dependency != target {
                mutation.require_unchanged(dependency, raw);
            }
        }
        match legacy_raw {
            Some(raw) => mutation.require_unchanged(legacy_path, raw),
            None => mutation.require_absent(legacy_path),
        };
        if already_refined {
            mutation.require_unchanged(target, before.expect("existing refinement"));
        } else {
            mutation
                .publish_expected(root, target, before, state.encode(self)?)
                .map_err(fail)?;
        }
        Ok((mutation, preview))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use lk_core::semantic::SemanticAuthority;

    fn fixture(session: &MutationSession) -> (Work, Vec<SemanticResource>) {
        let resources = vec![SemanticResource {
            address: "wiki/documents/source.md".into(),
            revision: "source-revision".into(),
            authority: SemanticAuthority::Page,
            text: "단계🙂 source paragraph.\n\n".repeat(900),
        }];
        let context = json!({"subject":{"kind":"audit", "path":"wiki/documents/source.md",
            "file_revision":"a".repeat(64), "input_revision":"b".repeat(64)},
            "purpose":{"kind":"audit", "locale":"en"}, "output_contract":"audit"});
        let work = Work::create(
            session,
            &resources,
            "subject",
            context,
            WorkLimits {
                max_input_bytes: 16000,
                max_result_bytes: 1200,
            },
        )
        .unwrap();
        (work, resources)
    }

    fn output(work: &Work, root: &Path, resources: &[SemanticResource], node: &str) -> WorkOutput {
        let request = work.input(root, node, resources).unwrap();
        let source_questions = if request["kind"] == "inspect" {
            vec![WorkQuestion {
                id: format!("{node}:source"),
                question: "The source leaves this fact unverified.".into(),
                evidence: vec![
                    serde_json::from_value(request["input"]["ranges"][0]["range"].clone()).unwrap(),
                ],
            }]
        } else {
            request["children"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|child| {
                    serde_json::from_value::<WorkOutput>(child["output"].clone())
                        .unwrap()
                        .source_questions
                })
                .collect()
        };
        WorkOutput {
            work_id: work.plan.id.clone(),
            node_id: node.into(),
            input_revision: request["input_revision"].as_str().unwrap().into(),
            summary: "Complete assigned observation.".into(),
            findings: Vec::new(),
            source_questions,
            unresolved: Vec::new(),
            resolutions: Vec::new(),
        }
    }

    fn hold(work: &Work, session: &MutationSession, resources: &[SemanticResource], node: &str) {
        let request = work.input(session.root(), node, resources).unwrap();
        let mutation = work
            .plan_inspection_action(
                session.root(),
                resources,
                &request,
                "contract",
                "Assigned inspection needs a smaller explicit unit.".into(),
            )
            .unwrap();
        session.apply(mutation).unwrap();
    }

    #[test]
    fn audit_refinement_resumes_without_rewriting_paid_state_or_losing_questions() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (work, resources) = fixture(&session);
        assert!(work.plan.parts.len() > 1);
        let first = work.plan.parts[0].id.clone();
        work.record(
            &session,
            output(&work, root.path(), &resources, &first),
            &resources,
        )
        .unwrap();
        let parent = work.plan.parts[1].id.clone();
        hold(&work, &session, &resources, &parent);
        let retained = [
            work_path(&work.plan.id, "plan.json").unwrap(),
            output_path(&work.plan.id, &first).unwrap(),
            work_path(&work.plan.id, "inspection-actions.json").unwrap(),
        ]
        .map(|path| {
            let raw = std::fs::read(root.path().join(&path)).unwrap();
            (path, raw)
        });
        let (mutation, preview) = work
            .plan_refinement(root.path(), &resources, &parent, 1000, |_| {
                Ok("contract".into())
            })
            .unwrap();
        assert!(!root.path().join(path(&work).unwrap()).exists());
        assert!(preview.inspection_nodes.len() > 1);
        session.apply(mutation).unwrap();
        let sidecar = std::fs::read(root.path().join(path(&work).unwrap())).unwrap();
        let (mutation, preview) = work
            .plan_refinement(root.path(), &resources, &parent, 1000, |_| {
                Ok("contract".into())
            })
            .unwrap();
        assert!(preview.already_refined);
        session.apply(mutation).unwrap();
        assert_eq!(
            sidecar,
            std::fs::read(root.path().join(path(&work).unwrap())).unwrap()
        );
        let child = preview.inspection_nodes[0].clone();
        hold(&work, &session, &resources, &child);
        let (mutation, nested) = work
            .plan_refinement(root.path(), &resources, &child, 400, |_| {
                Ok("contract".into())
            })
            .unwrap();
        session.apply(mutation).unwrap();
        assert!(nested.inspection_nodes.len() > 1);
        let restored = Work::load(root.path(), &work.plan.id).unwrap();
        assert_eq!(restored, work);
        loop {
            let progress = restored.progress(root.path(), &resources).unwrap();
            if progress.complete {
                break;
            }
            for node in progress.ready {
                restored
                    .record(
                        &session,
                        output(&restored, root.path(), &resources, &node),
                        &resources,
                    )
                    .unwrap();
            }
        }
        for (path, raw) in retained {
            assert_eq!(raw, std::fs::read(root.path().join(path)).unwrap());
        }
        let result = work.result(root.path(), &resources).unwrap();
        assert!(result.to_string().contains("unverified"));
        let nodes = work.capture_refinements(root.path()).unwrap().nodes;
        let outputs = nodes
            .iter()
            .map(|node| {
                let raw =
                    read_optional(root.path(), &output_path(&work.plan.id, &node.id).unwrap())
                        .unwrap()
                        .unwrap();
                let stored: StoredOutput = serde_json::from_str(&raw).unwrap();
                if node.refinement.is_some() {
                    assert!(raw.contains("work-output-refinement-v1"));
                }
                (node.id.clone(), stored)
            })
            .collect::<BTreeMap<_, _>>();
        work.verify_stored_dependencies(&outputs, &[nodes.last().unwrap().id.clone()], &nodes)
            .unwrap();
        assert!(
            outputs
                .values()
                .any(|output| output.refinement.is_some() && output.retained.is_some())
        );
        assert!(
            work.reuse_from(&session, &work, &parent, &parent, &resources)
                .is_err()
        );
        let final_result = json!({"body":"Published refined audit fixture"});
        work.seal(&session, &resources, final_result.clone())
            .unwrap();
        let mut publication = MutationPlan::default();
        work.guard_completion(&mut publication, root.path(), &resources, &final_result)
            .unwrap();
        work.plan_publication(&mut publication, root.path(), &final_result)
            .unwrap();
        publication
            .publish_expected(
                root.path(),
                work_path(&work.plan.id, "final.json").unwrap(),
                None,
                encoded(&final_result).unwrap(),
            )
            .unwrap();
        session.apply(publication).unwrap();
        let required_child = output_path(&work.plan.id, &nested.inspection_nodes[0]).unwrap();
        let child_raw = std::fs::read(root.path().join(&required_child)).unwrap();
        std::fs::remove_file(root.path().join(&required_child)).unwrap();
        assert!(prune_published(root.path(), None, i64::MAX).is_err());
        std::fs::write(root.path().join(&required_child), "[]").unwrap();
        assert!(prune_published(root.path(), Some(&session), i64::MAX).is_err());
        std::fs::write(root.path().join(&required_child), child_raw).unwrap();
        assert_eq!(
            prune_published(root.path(), None, i64::MAX)
                .unwrap()
                .removed,
            1
        );
        assert!(root.path().join(path(&work).unwrap()).is_file());
        assert_eq!(
            prune_published(root.path(), Some(&session), i64::MAX)
                .unwrap()
                .removed,
            1
        );
        assert!(!root.path().join(path(&work).unwrap()).exists());
        assert!(!root.path().join(required_child).exists());
    }

    #[test]
    fn refinement_requires_an_exact_hold_and_refuses_drift_and_invalid_coverage() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (work, resources) = fixture(&session);
        let node = &work.plan.parts[0].id;
        assert!(
            work.plan_refinement(root.path(), &resources, node, 1000, |_| Ok(
                "contract".into()
            ))
            .is_err()
        );
        hold(&work, &session, &resources, node);
        for bound in [0, usize::MAX] {
            assert!(
                work.plan_refinement(root.path(), &resources, node, bound, |_| Ok(
                    "contract".into()
                ))
                .is_err()
            );
        }
        assert!(
            work.plan_refinement(
                root.path(),
                &resources,
                node,
                1000,
                |_| Ok("changed".into())
            )
            .is_err()
        );
        let (mutation, _) = work
            .plan_refinement(root.path(), &resources, node, 1000, |_| {
                Ok("contract".into())
            })
            .unwrap();
        session.apply(mutation).unwrap();
        let file = root.path().join(path(&work).unwrap());
        let original = std::fs::read_to_string(&file).unwrap();
        let state: Refinements = serde_json::from_str(&original).unwrap();
        let mut invalid = state.clone();
        invalid.entries.get_mut(node).unwrap().parts[0].ranges[0].end -= 1;
        std::fs::write(&file, encoded(&invalid).unwrap()).unwrap();
        assert!(work.progress(root.path(), &resources).is_err());
        std::fs::write(
            &file,
            format!("{{\"schema\":\"{SCHEMA}\",{}", &original[1..]),
        )
        .unwrap();
        assert!(work.progress(root.path(), &resources).is_err());
        std::fs::write(&file, &original).unwrap();
        let legacy = root
            .path()
            .join(work_path(&work.plan.id, "inspection-actions.json").unwrap());
        let legacy_raw = std::fs::read(&legacy).unwrap();
        let mut changed_legacy = legacy_raw.clone();
        changed_legacy.push(b' ');
        std::fs::write(&legacy, changed_legacy).unwrap();
        assert!(work.progress(root.path(), &resources).is_err());
        std::fs::write(&legacy, legacy_raw).unwrap();
        let mut changed = resources.clone();
        changed[0].text.push('!');
        assert!(work.progress(root.path(), &changed).is_err());
        let progress = work.progress(root.path(), &resources).unwrap();
        let observation = output(&work, root.path(), &resources, &progress.ready[0]);
        let mutation = work
            .plan_record(&session, observation, &resources, None, Vec::new())
            .unwrap();
        std::fs::write(&file, format!("{original} ")).unwrap();
        assert!(session.apply(mutation).is_err());
    }

    #[test]
    fn refinement_actions_preserve_prior_reduction_bytes_after_a_corrected_child() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (work, resources) = fixture(&session);
        let parent = work.plan.parts[0].id.clone();
        hold(&work, &session, &resources, &parent);
        let (mutation, preview) = work
            .plan_refinement(root.path(), &resources, &parent, 1000, |_| {
                Ok("contract".into())
            })
            .unwrap();
        session.apply(mutation).unwrap();
        loop {
            let progress = work.progress(root.path(), &resources).unwrap();
            if progress.complete {
                break;
            }
            for node in progress.ready {
                work.record(
                    &session,
                    output(&work, root.path(), &resources, &node),
                    &resources,
                )
                .unwrap();
            }
        }
        let child = &preview.inspection_nodes[0];
        let mut corrected = output(&work, root.path(), &resources, child);
        corrected
            .summary
            .push_str(" Explicit corrected observation.");
        work.record(&session, corrected, &resources).unwrap();
        let progress = work.progress(root.path(), &resources).unwrap();
        let node = &progress.ready[0];
        let path = output_path(&work.plan.id, node).unwrap();
        let original = std::fs::read(root.path().join(&path)).unwrap();
        let request = work.input(root.path(), node, &resources).unwrap();
        assert_eq!(request["kind"], "reduce");
        let before = std::fs::read(root.path().join(super::path(&work).unwrap())).unwrap();
        let mut conflicting = work
            .plan_inspection_action(
                root.path(),
                &resources,
                &request,
                "contract",
                "A fresh exact hold.".into(),
            )
            .unwrap();
        conflicting.require_absent(path.clone());
        assert!(session.apply(conflicting).is_err());
        assert_eq!(
            before,
            std::fs::read(root.path().join(super::path(&work).unwrap())).unwrap()
        );
        hold(&work, &session, &resources, node);
        assert_eq!(original, std::fs::read(root.path().join(&path)).unwrap());
        assert!(
            work.load_inspection_action(root.path(), &request, "contract")
                .unwrap()
                .is_some()
        );
        assert!(
            work.prepare_inspection_admission(root.path(), &resources, |_| Ok("contract".into()))
                .unwrap()
                .1
                .is_some()
        );
    }

    #[test]
    fn interrupted_refinement_rechecks_the_original_action_and_resumes_its_own_edit() {
        for edit_receipted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let (work, resources) = fixture(&session);
            let node = &work.plan.parts[0].id;
            hold(&work, &session, &resources, node);
            let (plan, _) = work
                .plan_refinement(root.path(), &resources, node, 1000, |_| {
                    Ok("contract".into())
                })
                .unwrap();
            let plan_revision = lk_core::concept::file_revision(&encoded(&plan).unwrap());
            let plan = serde_json::to_value(plan).unwrap();
            let steps = plan["steps"].as_array().unwrap();
            let edit = steps.last().unwrap();
            assert!(
                edit["path"]
                    .as_str()
                    .unwrap()
                    .ends_with("inspection-refinements.json")
            );
            let completed = steps.len() - usize::from(!edit_receipted);
            drop(session);
            let directory = root.path().join(".lorekeeper/mutation");
            std::fs::write(
                directory.join("pending.json"),
                encoded(&json!({"version":1,"plan":plan})).unwrap(),
            )
            .unwrap();
            let sidecar = root.path().join(edit["path"].as_str().unwrap());
            std::fs::write(&sidecar, edit["after"].as_str().unwrap()).unwrap();
            std::fs::write(directory.join("progress.json"),encoded(&json!({"plan_revision":plan_revision,
                "completed":completed,"checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{completed}"))})).unwrap()).unwrap();
            let legacy = root
                .path()
                .join(work_path(&work.plan.id, "inspection-actions.json").unwrap());
            let original = std::fs::read(&legacy).unwrap();
            let mut changed = original.clone();
            changed.push(b' ');
            std::fs::write(&legacy, changed).unwrap();
            assert!(MutationSession::acquire(root.path()).is_err());
            std::fs::write(&legacy, &original).unwrap();
            let _recovered = MutationSession::acquire(root.path()).unwrap();
            assert!(!directory.join("pending.json").exists());
            assert!(work.progress(root.path(), &resources).unwrap().ready.len() > 1);
            assert_eq!(original, std::fs::read(&legacy).unwrap());
        }
    }

    #[test]
    fn refinement_reserves_large_declared_results_without_allocating_the_allowance() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let (ordinary, mut resources) = fixture(&session);
        resources[0].text = "abc🙂def".into();
        let work = Work::create(
            &session,
            &resources,
            "wide-capacity",
            ordinary.context,
            WorkLimits {
                max_input_bytes: usize::MAX,
                max_result_bytes: usize::MAX / 8,
            },
        )
        .unwrap();
        let node = &work.plan.parts[0].id;
        hold(&work, &session, &resources, node);
        let (mutation, preview) = work
            .plan_refinement(root.path(), &resources, node, 2, |_| Ok("contract".into()))
            .unwrap();
        assert!(preview.inspection_nodes.len() > 1);
        session.apply(mutation).unwrap();
        assert!(
            work.prepare_inspections(root.path(), &resources, 4)
                .unwrap()
                .1
                .len()
                > 1
        );
    }
}
