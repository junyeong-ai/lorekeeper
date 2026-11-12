use super::*;

const MAX_TIMEOUTS: usize = 2;

#[cfg(unix)]
pub(crate) enum TimeoutUpdate {
    Record,
    Retry,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectionTimeouts {
    schema: Schema,
    work_id: String,
    #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
    entries: BTreeMap<String, Timeout>,
}

#[derive(Serialize, Deserialize)]
enum Schema {
    #[serde(rename = "inspection-timeouts-v1")]
    V1,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Timeout {
    request_revision: String,
    count: usize,
}

pub(super) fn path(work: &Work) -> miette::Result<PathBuf> {
    work_path(&work.plan.id, "inspection-timeouts.json")
}

impl InspectionTimeouts {
    pub(super) fn decode(raw: &str, work: &Work, nodes: &[Node]) -> miette::Result<Self> {
        let value: Self = super::super::receipt::decode(raw).map_err(fail)?;
        if raw.len() > work.max_input_bytes || value.work_id != work.plan.id {
            return Err(fail("invalid inspection timeout identity or bound"));
        }
        for (id, entry) in &value.entries {
            require_id(&entry.request_revision)?;
            if !nodes.iter().any(|node| &node.id == id)
                || entry.count == 0
                || entry.count > MAX_TIMEOUTS
            {
                return Err(fail("invalid inspection timeout node or count"));
            }
        }
        Ok(value)
    }

    pub(super) fn load(root: &Path, work: &Work, nodes: &[Node]) -> miette::Result<Option<Self>> {
        read_optional(root, &path(work)?)?
            .map(|raw| Self::decode(&raw, work, nodes))
            .transpose()
    }

    pub(super) fn has_node(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    pub(super) fn reason(&self, request: &Value, contract: &str) -> miette::Result<Option<String>> {
        let id = request["node_id"]
            .as_str()
            .ok_or_else(|| fail("inspection has no node"))?;
        let revision = inspection::request_revision(request, contract)?;
        Ok(self.entries.get(id).filter(|entry| entry.request_revision == revision && entry.count == MAX_TIMEOUTS)
            .map(|_| format!("inspection {id} reached its {MAX_TIMEOUTS} session timeouts; diagnose the host, then use lore work retry {} {id}", self.work_id)))
    }
}

impl Work {
    #[cfg(unix)]
    pub fn plan_inspection_timeout(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        update: TimeoutUpdate,
    ) -> miette::Result<(MutationPlan, usize)> {
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated
            .nodes
            .iter()
            .find(|node| request["node_id"].as_str() == Some(node.id.as_str()))
            .ok_or_else(|| fail("inspection timeout names an unknown node"))?;
        if !evaluated.ready.contains(&node.id)
            || self
                .node_input(node, &evaluated.inputs, &evaluated.current)
                .map_err(NodeInputError::report)?
                != *request
        {
            return Err(fail(
                "inspection timeout requires its exact unfinished ready request",
            ));
        }
        let path = path(self)?;
        let before = read_optional(root, &path)?;
        let mut timeouts = match &before {
            Some(raw) => InspectionTimeouts::decode(raw, self, &evaluated.nodes)?,
            None => InspectionTimeouts {
                schema: Schema::V1,
                work_id: self.plan.id.clone(),
                entries: BTreeMap::new(),
            },
        };
        let revision = inspection::request_revision(request, contract)?;
        let count = timeouts
            .entries
            .get(&node.id)
            .filter(|entry| entry.request_revision == revision)
            .map_or(0, |entry| entry.count);
        let remaining = if matches!(update, TimeoutUpdate::Retry) {
            if count != MAX_TIMEOUTS {
                return Err(fail(
                    "inspection has no current exhausted timeout allowance",
                ));
            }
            timeouts.entries.remove(&node.id);
            MAX_TIMEOUTS
        } else {
            if count == MAX_TIMEOUTS {
                return Err(fail("inspection timeout allowance is already exhausted"));
            }
            timeouts.entries.insert(
                node.id.clone(),
                Timeout {
                    request_revision: revision,
                    count: count + 1,
                },
            );
            MAX_TIMEOUTS - count - 1
        };
        let after = encoded(&timeouts)?;
        if after.len() > self.max_input_bytes {
            return Err(fail("inspection timeouts exceed their storage bound"));
        }
        let mut mutation = MutationPlan::default();
        let output = output_path(&self.plan.id, &node.id)?;
        if !evaluated.raw.contains_key(&output) {
            mutation.require_absent(output);
        }
        if evaluated.refinements.is_none() {
            mutation.require_absent(refinement::path(self)?);
        }
        for (path, raw) in evaluated.raw {
            mutation.require_unchanged(path, raw);
        }
        mutation
            .publish_expected(root, path, before, after)
            .map_err(fail)?;
        Ok((mutation, remaining))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::{context, limits, resources};
    use super::*;

    #[test]
    fn timeouts_bind_exact_requests_and_retry_preserves_semantic_holds() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let request = work
            .prepare_inspection(root.path(), &source)
            .unwrap()
            .1
            .unwrap();
        let contract = "contract";
        for expected in [1, 0] {
            let (plan, remaining) = work
                .plan_inspection_timeout(
                    root.path(),
                    &source,
                    &request,
                    contract,
                    TimeoutUpdate::Record,
                )
                .unwrap();
            assert_eq!(remaining, expected);
            session.apply(plan).unwrap();
            assert_eq!(
                work.load_inspection_hold(root.path(), &request, contract)
                    .unwrap()
                    .is_some(),
                expected == 0
            );
        }
        assert!(
            work.plan_inspection_timeout(
                root.path(),
                &source,
                &request,
                contract,
                TimeoutUpdate::Record
            )
            .is_err()
        );
        assert!(
            work.load_inspection_hold(root.path(), &request, "different-contract")
                .unwrap()
                .is_none()
        );
        let semantic = work
            .plan_inspection_action(
                root.path(),
                &source,
                &request,
                contract,
                "An explicit source decision is required.".into(),
            )
            .unwrap();
        session.apply(semantic).unwrap();
        let paid = read_optional(
            root.path(),
            &work_path(&work.plan.id, "inspection-actions.json").unwrap(),
        )
        .unwrap();
        let (retry, _) = work
            .plan_inspection_timeout(
                root.path(),
                &source,
                &request,
                contract,
                TimeoutUpdate::Retry,
            )
            .unwrap();
        session.apply(retry).unwrap();
        assert_eq!(
            read_optional(
                root.path(),
                &work_path(&work.plan.id, "inspection-actions.json").unwrap()
            )
            .unwrap(),
            paid
        );
        assert_eq!(
            work.load_inspection_hold(root.path(), &request, contract)
                .unwrap()
                .as_deref(),
            Some("An explicit source decision is required.")
        );
        assert!(
            work.plan_inspection_timeout(
                root.path(),
                &source,
                &request,
                contract,
                TimeoutUpdate::Retry
            )
            .is_err()
        );
        assert_eq!(work.progress(root.path(), &source).unwrap().completed, 0);
    }

    #[test]
    fn corrupt_timeout_records_refuse_without_becoming_an_empty_allowance() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let request = work
            .prepare_inspection(root.path(), &source)
            .unwrap()
            .1
            .unwrap();
        let (plan, _) = work
            .plan_inspection_timeout(
                root.path(),
                &source,
                &request,
                "contract",
                TimeoutUpdate::Record,
            )
            .unwrap();
        session.apply(plan).unwrap();
        let path = root.path().join(path(&work).unwrap());
        let raw = std::fs::read(&path).unwrap();
        for field in ["schema", "work_id", "count", "node"] {
            let mut value: Value = serde_json::from_slice(&raw).unwrap();
            let node = request["node_id"].as_str().unwrap();
            match field {
                "schema" => value["schema"] = json!("unknown"),
                "work_id" => value["work_id"] = json!("f".repeat(64)),
                "count" => value["entries"][node]["count"] = json!(MAX_TIMEOUTS + 1),
                "node" => {
                    let entry = value["entries"]
                        .as_object_mut()
                        .unwrap()
                        .remove(node)
                        .unwrap();
                    value["entries"]["f".repeat(64)] = entry;
                }
                _ => unreachable!(),
            }
            let invalid = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &invalid).unwrap();
            assert!(
                work.load_inspection_hold(root.path(), &request, "contract")
                    .is_err()
            );
            assert!(
                work.plan_inspection_timeout(
                    root.path(),
                    &source,
                    &request,
                    "contract",
                    TimeoutUpdate::Retry
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), invalid);
        }
    }
}
