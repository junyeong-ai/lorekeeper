use super::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputCapacity {
    pub actual_bytes: usize,
    pub max_bytes: usize,
}

impl std::fmt::Display for OutputCapacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "worker output requires {} bytes, above its {} byte allowance; no observation was recorded",
            self.actual_bytes, self.max_bytes
        )
    }
}

impl std::error::Error for OutputCapacity {}
impl miette::Diagnostic for OutputCapacity {}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectionActions {
    schema: String,
    work_id: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    actions: BTreeMap<String, InspectionAction>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectionAction {
    pub input_revision: String,
    pub request_revision: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<OutputCapacity>,
}

impl InspectionAction {
    pub(super) fn validate(&self, work: &Work) -> miette::Result<()> {
        require_id(&self.input_revision)?;
        require_id(&self.request_revision)?;
        nonblank(&self.reason)?;
        if let Some(capacity) = &self.capacity
            && (capacity.max_bytes == 0
                || capacity.max_bytes > work.plan.max_result_bytes
                || capacity.actual_bytes <= capacity.max_bytes)
        {
            return Err(fail("invalid inspection capacity rejection"));
        }
        Ok(())
    }
}

impl InspectionActions {
    #[cfg(unix)]
    pub(super) fn matching_action(
        &self,
        work: &Work,
        request: &Value,
        contract: &str,
    ) -> miette::Result<Option<InspectionAction>> {
        if self.reason(work, request, contract)?.is_none() {
            return Ok(None);
        }
        Ok(self
            .actions
            .get(request["node_id"].as_str().expect("validated node"))
            .cloned())
    }
    pub(super) fn decode(raw: &str, work: &Work) -> miette::Result<Self> {
        let actions: Self = crate::commands::work::receipt::decode(raw).map_err(fail)?;
        if !matches!(
            actions.schema.as_str(),
            "inspection-actions-v1" | "inspection-actions-v2"
        ) || actions.work_id != work.plan.id
            || actions.actions.is_empty()
        {
            return Err(fail("invalid inspection action receipt identity or schema"));
        }
        let nodes = work.nodes()?;
        for (node, action) in &actions.actions {
            if !nodes.iter().any(|candidate| &candidate.id == node) {
                return Err(fail("inspection action names an unknown work node"));
            }
            require_id(&action.input_revision)?;
            require_id(&action.request_revision)?;
            nonblank(&action.reason)?;
            if let Some(capacity) = &action.capacity
                && (actions.schema != "inspection-actions-v2"
                    || capacity.max_bytes == 0
                    || capacity.max_bytes > work.plan.max_result_bytes
                    || capacity.actual_bytes <= capacity.max_bytes)
            {
                return Err(fail("invalid inspection capacity rejection"));
            }
        }
        Ok(actions)
    }

    fn reason(
        &self,
        work: &Work,
        request: &Value,
        contract: &str,
    ) -> miette::Result<Option<String>> {
        let Some(node) = request["node_id"].as_str() else {
            return Err(fail("inspection request has no node identity"));
        };
        let Some(action) = self.actions.get(node) else {
            return Ok(None);
        };
        if action.request_revision != request_revision(request, contract)? {
            return Ok(None);
        }
        if request["work_id"] != work.plan.id
            || request["input_revision"].as_str() != Some(action.input_revision.as_str())
        {
            return Err(fail("inspection action identity differs from its request"));
        }
        Ok(Some(action.reason.clone()))
    }
}

pub(super) fn request_revision(request: &Value, contract: &str) -> miette::Result<String> {
    revision(&json!({"request":request,"contract":contract}))
}

impl Work {
    pub fn load_inspection_action(
        &self,
        root: &Path,
        request: &Value,
        contract: &str,
    ) -> miette::Result<Option<String>> {
        let captured = self.capture_refinements(root)?;
        if let Some(refinements) = captured.state
            && let Some(action) = refinements.action(self, request, contract)?
        {
            return Ok(Some(action.reason));
        }
        let Some(raw) = read_optional(root, &work_path(&self.plan.id, "inspection-actions.json")?)?
        else {
            return Ok(None);
        };
        let actions = InspectionActions::decode(&raw, self)?;
        actions.reason(self, request, contract)
    }

    pub fn prepare_inspection_admission(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        contract: impl Fn(&Value) -> miette::Result<String>,
    ) -> miette::Result<(WorkProgress, Option<String>)> {
        let actions = read_optional(root, &work_path(&self.plan.id, "inspection-actions.json")?)?
            .map(|raw| InspectionActions::decode(&raw, self))
            .transpose()?;
        let evaluated = self.evaluate(root, resources)?;
        let progress = evaluated.progress();
        for id in &progress.ready {
            if progress.ready.first() != Some(id)
                && !actions
                    .as_ref()
                    .is_some_and(|actions| actions.actions.contains_key(id))
                && !evaluated
                    .refinements
                    .as_ref()
                    .is_some_and(|state| state.has_action_for_node(id))
            {
                continue;
            }
            let node = evaluated
                .nodes
                .iter()
                .find(|node| &node.id == id)
                .expect("ready node belongs to the evaluated tree");
            let request = self
                .node_input(node, &evaluated.inputs, &evaluated.current)
                .map_err(NodeInputError::report)?;
            if let Some(refinements) = &evaluated.refinements
                && let Some(action) = refinements.action(self, &request, &contract(&request)?)?
            {
                return Ok((progress, Some(action.reason)));
            }
            if let Some(actions) = &actions
                && let Some(reason) = actions.reason(self, &request, &contract(&request)?)?
            {
                return Ok((progress, Some(reason)));
            }
        }
        if !progress.complete
            && let Some(refinements) = &evaluated.refinements
        {
            refinements.available_slots(self)?;
        }
        Ok((progress, None))
    }

    pub fn plan_inspection_action(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        reason: String,
    ) -> miette::Result<MutationPlan> {
        self.plan_inspection_block(root, resources, request, contract, reason, None)
    }

    pub fn plan_inspection_capacity(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        capacity: OutputCapacity,
    ) -> miette::Result<MutationPlan> {
        if capacity.max_bytes == 0
            || capacity.max_bytes > self.plan.max_result_bytes
            || capacity.actual_bytes <= capacity.max_bytes
        {
            return Err(fail("invalid inspection capacity rejection"));
        }
        self.plan_inspection_block(
            root,
            resources,
            request,
            contract,
            capacity.to_string(),
            Some(capacity),
        )
    }

    fn plan_inspection_block(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        reason: String,
        capacity: Option<OutputCapacity>,
    ) -> miette::Result<MutationPlan> {
        nonblank(&reason)?;
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated
            .nodes
            .iter()
            .find(|node| request["node_id"].as_str() == Some(node.id.as_str()))
            .ok_or_else(|| fail("inspection action names an unknown node"))?;
        if !evaluated.ready.contains(&node.id) {
            return Err(fail("inspection action requires an unfinished ready node"));
        }
        let current = self
            .node_input(node, &evaluated.inputs, &evaluated.current)
            .map_err(NodeInputError::report)?;
        if &current != request {
            return Err(fail("inspection action request changed before recording"));
        }
        let action = InspectionAction {
            input_revision: current["input_revision"]
                .as_str()
                .expect("native input revision")
                .into(),
            request_revision: request_revision(request, contract)?,
            reason,
            capacity,
        };
        if let Some(refinements) = evaluated.refinements.clone() {
            return refinements.plan_action(self, root, request, action, evaluated);
        }
        let path = work_path(&self.plan.id, "inspection-actions.json")?;
        let before = read_optional(root, &path)?;
        let mut actions = match &before {
            Some(raw) => InspectionActions::decode(raw, self)?,
            None => InspectionActions {
                schema: "inspection-actions-v1".into(),
                work_id: self.plan.id.clone(),
                actions: BTreeMap::new(),
            },
        };
        if action.capacity.is_some() {
            actions.schema = "inspection-actions-v2".into();
        }
        actions.actions.insert(node.id.clone(), action);
        let mut plan = MutationPlan::default();
        let output = output_path(&self.plan.id, &node.id)?;
        if !evaluated.raw.contains_key(&output) {
            plan.require_absent(output);
        }
        for (path, raw) in evaluated.raw {
            plan.require_unchanged(path, raw);
        }
        plan.publish_expected(root, path, before, encoded(&actions)?)
            .map_err(fail)?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{context, limits, resources};
    use super::*;

    #[test]
    fn admission_checks_exact_actions_on_current_ready_nodes_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let (progress, requests) = work.prepare_inspections(root.path(), &source, 2).unwrap();
        assert_eq!(requests.len(), 2);
        let request = &requests[1];
        session
            .apply(
                work.plan_inspection_action(
                    root.path(),
                    &source,
                    request,
                    "contract-1",
                    "Explicit action.".into(),
                )
                .unwrap(),
            )
            .unwrap();
        let path = root
            .path()
            .join(work_path(&work.plan.id, "inspection-actions.json").unwrap());
        let before = std::fs::read(&path).unwrap();
        let (current, hold) = work
            .prepare_inspection_admission(root.path(), &source, |_| Ok("contract-1".into()))
            .unwrap();
        assert_eq!(current.ready, progress.ready);
        assert_eq!(current.completed, progress.completed);
        assert_eq!(hold.as_deref(), Some("Explicit action."));
        assert!(
            work.prepare_inspection_admission(root.path(), &source, |_| Ok("contract-2".into()),)
                .unwrap()
                .1
                .is_none()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::write(&path, "[]").unwrap();
        assert!(
            work.prepare_inspection_admission(root.path(), &source, |_| Ok("contract-1".into()),)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            work.prepare_inspection_admission(root.path(), &source, |_| Ok("contract-1".into()),)
                .is_err()
        );
    }

    #[test]
    fn actions_bind_each_exact_ready_request_and_preserve_other_nodes() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let manifest = std::fs::read(
            root.path()
                .join(work_path(&work.plan.id, "plan.json").unwrap()),
        )
        .unwrap();
        let (progress, requests) = work.prepare_inspections(root.path(), &source, 2).unwrap();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            let plan = work
                .plan_inspection_action(
                    root.path(),
                    &source,
                    request,
                    "contract-1",
                    "Explicit processing action.".into(),
                )
                .unwrap();
            session.apply(plan).unwrap();
        }
        for request in &requests {
            assert!(
                work.load_inspection_action(root.path(), request, "contract-1")
                    .unwrap()
                    .is_some()
            );
            assert!(
                work.load_inspection_action(root.path(), request, "contract-2")
                    .unwrap()
                    .is_none()
            );
        }
        let mut changed = requests[0].clone();
        changed["context"]["purpose"]["locale"] = json!("en");
        assert!(
            work.load_inspection_action(root.path(), &changed, "contract-1")
                .unwrap()
                .is_none()
        );
        assert!(
            work.plan_inspection_action(
                root.path(),
                &source,
                &changed,
                "contract-1",
                "Must refuse.".into()
            )
            .is_err()
        );
        let after = work.progress(root.path(), &source).unwrap();
        assert_eq!(after.completed, progress.completed);
        assert!(!after.complete);
        assert_eq!(
            std::fs::read(
                root.path()
                    .join(work_path(&work.plan.id, "plan.json").unwrap())
            )
            .unwrap(),
            manifest
        );
        let path = root
            .path()
            .join(work_path(&work.plan.id, "inspection-actions.json").unwrap());
        let original = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&original).unwrap();
        let raw = serde_json::to_string(&value).unwrap();
        let needle = "\"reason\":\"Explicit processing action.\"";
        assert!(raw.contains(needle));
        let node = requests[0]["node_id"].as_str().unwrap();
        let duplicate_field = raw.replacen(needle, &format!("\"reason\":null,{needle}"), 1);
        let duplicate_node = raw.replacen(
            "\"actions\":{",
            &format!(
                "\"actions\":{{{}:{},",
                serde_json::to_string(node).unwrap(),
                value["actions"][node]
            ),
            1,
        );
        for corrupt in [duplicate_field, duplicate_node] {
            std::fs::write(&path, &corrupt).unwrap();
            assert!(
                work.load_inspection_action(root.path(), &requests[0], "contract-1")
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
        }
        std::fs::write(&path, &original).unwrap();
        for corrupt in [json!([]), json!({}), {
            let mut value: Value = serde_json::from_str(&original).unwrap();
            value["actions"] = json!({});
            value
        }] {
            std::fs::write(&path, encoded(&corrupt).unwrap()).unwrap();
            assert!(
                work.load_inspection_action(root.path(), &requests[0], "contract-2")
                    .is_err()
            );
            assert!(
                work.plan_inspection_action(
                    root.path(),
                    &source,
                    &requests[0],
                    "contract-2",
                    "Action.".into()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn interrupted_actions_recheck_absent_nodes_and_preserved_dependencies() {
        for edit_receipted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let session = MutationSession::acquire(root.path()).unwrap();
            let source = resources();
            let work =
                Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
            let (_, request) = work.prepare_inspection(root.path(), &source).unwrap();
            let request = request.unwrap();
            let plan = work
                .plan_inspection_action(
                    root.path(),
                    &source,
                    &request,
                    "contract",
                    "Action required.".into(),
                )
                .unwrap();
            let plan_revision = lk_core::concept::file_revision(&encoded(&plan).unwrap());
            let plan = serde_json::to_value(plan).unwrap();
            let steps = plan["steps"].as_array().unwrap();
            let edit = steps.last().unwrap();
            let path = root.path().join(edit["path"].as_str().unwrap());
            let completed = steps.len() - usize::from(!edit_receipted);
            drop(session);
            let directory = root.path().join(".lorekeeper/mutation");
            std::fs::write(
                directory.join("pending.json"),
                encoded(&json!({"version":1,"plan":plan})).unwrap(),
            )
            .unwrap();
            std::fs::write(&path, edit["after"].as_str().unwrap()).unwrap();
            std::fs::write(directory.join("progress.json"), encoded(&json!({
                "plan_revision":plan_revision,"completed":completed,
                "checksum":lk_core::concept::file_revision(&format!("{plan_revision}:{completed}")),
            })).unwrap()).unwrap();
            let node = root
                .path()
                .join(output_path(&work.plan.id, request["node_id"].as_str().unwrap()).unwrap());
            std::fs::create_dir_all(node.parent().unwrap()).unwrap();
            std::fs::write(&node, "Unexpected concurrent observation.").unwrap();
            assert!(MutationSession::acquire(root.path()).is_err());
            std::fs::remove_file(&node).unwrap();
            let _recovered = MutationSession::acquire(root.path()).unwrap();
            assert!(!directory.join("pending.json").exists());
            assert!(
                work.load_inspection_action(root.path(), &request, "contract")
                    .unwrap()
                    .is_some()
            );
            assert_eq!(work.progress(root.path(), &source).unwrap().completed, 0);
        }
    }
}
