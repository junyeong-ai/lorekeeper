use super::*;

const MAX_TIMEOUTS: usize = 2;
const MAX_HOLD_REASON_BYTES: usize = 256;

fn storage_bound(nodes: &[Node]) -> miette::Result<usize> {
    nodes
        .len()
        .checked_add(1)
        .and_then(|count| count.checked_mul(1024))
        .ok_or_else(|| fail("inspection attempt storage bound overflow"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum InvocationFailure {
    TimedOut {
        engine: lk_core::execution::AgentEngine,
    },
    Exited {
        engine: lk_core::execution::AgentEngine,
        code: i32,
    },
    InvalidResponse {
        engine: lk_core::execution::AgentEngine,
        revision: String,
        bytes: usize,
    },
    MissingResponse {
        engine: lk_core::execution::AgentEngine,
    },
}

impl InvocationFailure {
    pub(crate) fn validate(&self) -> miette::Result<()> {
        match self {
            Self::Exited { code, .. } if !(1..=255).contains(code) => {
                Err(fail("invalid invocation exit code"))
            }
            Self::InvalidResponse { revision, .. } => require_id(revision),
            _ => Ok(()),
        }
    }

    /// Whether the host ended without any answer, as distinct from answering with an unusable
    /// one: an empty response is no answer.
    pub(crate) fn is_unanswered(&self) -> bool {
        !matches!(self, Self::InvalidResponse { bytes, .. } if *bytes > 0)
    }
}

impl std::fmt::Display for InvocationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut { .. } => f.write_str("native invocation reached its session timeout"),
            Self::Exited { code, .. } => write!(
                f,
                "native invocation exited with status {code}; no provider cause was classified"
            ),
            Self::InvalidResponse { .. } => {
                f.write_str("native invocation returned an unusable response")
            }
            Self::MissingResponse { .. } => {
                f.write_str("native invocation returned no required response")
            }
        }
    }
}

#[cfg(unix)]
pub(crate) enum AttemptUpdate {
    TimedOut,
    Failed(InvocationFailure),
    Hold(String),
    Retry,
}

#[derive(Serialize)]
pub(super) struct InspectionAttempts {
    schema: Schema,
    work_id: String,
    entries: BTreeMap<String, Attempt>,
}

#[derive(Serialize)]
enum Schema {
    #[serde(rename = "inspection-attempts-v2")]
    V2,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Attempt {
    Timeout {
        request_revision: String,
        count: usize,
    },
    Failed {
        canonical_revision: String,
        request_revision: String,
        failure: InvocationFailure,
    },
    Held {
        canonical_revision: String,
        reason: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Timeout {
    request_revision: String,
    count: usize,
}

#[derive(Deserialize)]
#[serde(tag = "schema", deny_unknown_fields)]
enum StoredAttempts {
    #[serde(rename = "inspection-timeouts-v1")]
    Timeouts {
        work_id: String,
        #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
        entries: BTreeMap<String, Timeout>,
    },
    #[serde(rename = "inspection-attempts-v2")]
    Attempts {
        work_id: String,
        #[serde(deserialize_with = "crate::commands::work::receipt::unique_entries")]
        entries: BTreeMap<String, Attempt>,
    },
}

pub(super) fn path(work: &Work) -> miette::Result<PathBuf> {
    work_path(&work.plan.id, "inspection-timeouts.json")
}

impl InspectionAttempts {
    #[cfg(unix)]
    fn new(work_id: String) -> Self {
        Self {
            schema: Schema::V2,
            work_id,
            entries: BTreeMap::new(),
        }
    }

    pub(super) fn decode(raw: &str, work: &Work, nodes: &[Node]) -> miette::Result<Self> {
        if raw.len() > storage_bound(nodes)? {
            return Err(fail("inspection attempts exceed their storage bound"));
        }
        let stored: StoredAttempts = super::super::receipt::decode(raw).map_err(fail)?;
        let value = match stored {
            StoredAttempts::Timeouts { work_id, entries } => Self {
                schema: Schema::V2,
                work_id,
                entries: entries
                    .into_iter()
                    .map(|(id, timeout)| {
                        (
                            id,
                            Attempt::Timeout {
                                request_revision: timeout.request_revision,
                                count: timeout.count,
                            },
                        )
                    })
                    .collect(),
            },
            StoredAttempts::Attempts { work_id, entries } => Self {
                schema: Schema::V2,
                work_id,
                entries,
            },
        };
        if value.work_id != work.plan.id {
            return Err(fail("invalid inspection attempt identity"));
        }
        for (id, entry) in &value.entries {
            if !nodes.iter().any(|node| &node.id == id) {
                return Err(fail("inspection attempt names an unknown node"));
            }
            match entry {
                Attempt::Timeout {
                    request_revision,
                    count,
                } => {
                    require_id(request_revision)?;
                    if *count == 0 || *count > MAX_TIMEOUTS {
                        return Err(fail("invalid inspection timeout count"));
                    }
                }
                Attempt::Failed {
                    canonical_revision,
                    request_revision,
                    failure,
                } => {
                    require_id(canonical_revision)?;
                    require_id(request_revision)?;
                    failure.validate()?;
                }
                Attempt::Held {
                    canonical_revision,
                    reason,
                } => {
                    require_id(canonical_revision)?;
                    validate_reason(reason)?;
                }
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
        let Some(entry) = self.entries.get(id) else {
            return Ok(None);
        };
        let reason = match entry {
            Attempt::Timeout {
                request_revision,
                count,
            } if *request_revision == inspection::request_revision(request, contract)?
                && *count == MAX_TIMEOUTS =>
            {
                Some(format!(
                    "inspection {id} reached its {MAX_TIMEOUTS} session timeouts"
                ))
            }
            Attempt::Failed {
                canonical_revision,
                failure,
                ..
            } if *canonical_revision == revision(request)? => Some(failure.to_string()),
            Attempt::Held {
                canonical_revision,
                reason,
            } if *canonical_revision == revision(request)? => Some(reason.clone()),
            _ => None,
        };
        Ok(reason.map(|reason| {
            format!(
                "{reason}; diagnose the host, then use lore work retry {} {id}",
                self.work_id
            )
        }))
    }
}

fn validate_reason(reason: &str) -> miette::Result<()> {
    nonblank(reason)?;
    if serde_json::to_vec(reason).map_err(fail)?.len() > MAX_HOLD_REASON_BYTES {
        return Err(fail(
            "inspection hold reason exceeds its compact JSON allowance",
        ));
    }
    Ok(())
}

impl Work {
    #[cfg(unix)]
    pub fn plan_inspection_attempt(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        update: AttemptUpdate,
    ) -> miette::Result<(MutationPlan, usize)> {
        let evaluated = self.evaluate(root, resources)?;
        let node = evaluated
            .nodes
            .iter()
            .find(|node| request["node_id"].as_str() == Some(node.id.as_str()))
            .ok_or_else(|| fail("inspection attempt names an unknown node"))?;
        if !evaluated.ready.contains(&node.id)
            || self
                .node_input(node, &evaluated.inputs, &evaluated.current)
                .map_err(NodeInputError::report)?
                != *request
        {
            return Err(fail(
                "inspection attempt requires its exact unfinished ready request",
            ));
        }
        let path = path(self)?;
        let before = read_optional(root, &path)?;
        let mut attempts = match &before {
            Some(raw) => InspectionAttempts::decode(raw, self, &evaluated.nodes)?,
            None => InspectionAttempts::new(self.plan.id.clone()),
        };
        let request_revision = inspection::request_revision(request, contract)?;
        let canonical_revision = revision(request)?;
        let current = attempts.entries.get(&node.id);
        let remaining = match update {
            AttemptUpdate::Retry => {
                let retryable = match current {
                    Some(Attempt::Timeout {
                        request_revision: previous,
                        count,
                    }) => previous == &request_revision && *count == MAX_TIMEOUTS,
                    Some(
                        Attempt::Failed {
                            canonical_revision: previous,
                            ..
                        }
                        | Attempt::Held {
                            canonical_revision: previous,
                            ..
                        },
                    ) => previous == &canonical_revision,
                    None => false,
                };
                if !retryable {
                    return Err(fail(
                        "inspection has no current exhausted timeout or failed invocation",
                    ));
                }
                attempts.entries.remove(&node.id);
                MAX_TIMEOUTS
            }
            AttemptUpdate::TimedOut => {
                if attempts.reason(request, contract)?.is_some() {
                    return Err(fail(
                        "inspection timeout allowance is already exhausted or held",
                    ));
                }
                let count = match current {
                    Some(Attempt::Timeout {
                        request_revision: previous,
                        count,
                    }) if previous == &request_revision => *count,
                    _ => 0,
                };
                attempts.entries.insert(
                    node.id.clone(),
                    Attempt::Timeout {
                        request_revision,
                        count: count + 1,
                    },
                );
                MAX_TIMEOUTS - count - 1
            }
            AttemptUpdate::Failed(failure) => {
                failure.validate()?;
                if attempts.reason(request, contract)?.is_some() {
                    return Err(fail("inspection already has a current operational hold"));
                }
                attempts.entries.insert(
                    node.id.clone(),
                    Attempt::Failed {
                        canonical_revision,
                        request_revision,
                        failure,
                    },
                );
                0
            }
            AttemptUpdate::Hold(reason) => {
                validate_reason(&reason)?;
                if attempts.reason(request, contract)?.is_some() {
                    return Err(fail("inspection already has a current operational hold"));
                }
                attempts.entries.insert(
                    node.id.clone(),
                    Attempt::Held {
                        canonical_revision,
                        reason,
                    },
                );
                0
            }
        };
        let after = encoded(&attempts)?;
        if after.len() > storage_bound(&evaluated.nodes)? {
            return Err(fail("inspection attempts exceed their storage bound"));
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
    fn operational_holds_survive_contract_changes_without_authorizing_refinement() {
        for update in [
            AttemptUpdate::Failed(InvocationFailure::Exited {
                engine: lk_core::execution::AgentEngine::Codex,
                code: 1,
            }),
            AttemptUpdate::Hold("Review the failed host before another invocation.".into()),
        ] {
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
            let (plan, remaining) = work
                .plan_inspection_attempt(root.path(), &source, &request, "old-contract", update)
                .unwrap();
            assert_eq!(remaining, 0);
            session.apply(plan).unwrap();
            for contract in ["old-contract", "new-contract"] {
                assert!(
                    work.load_inspection_hold(root.path(), &request, contract)
                        .unwrap()
                        .is_some()
                );
            }
            for path in ["inspection-actions.json", "inspection-refinements.json"] {
                assert!(
                    read_optional(root.path(), &work_path(&work.plan.id, path).unwrap())
                        .unwrap()
                        .is_none()
                );
            }
            let mut different = request.clone();
            different["context"]["different"] = json!(true);
            assert!(
                work.load_inspection_hold(root.path(), &different, "new-contract")
                    .unwrap()
                    .is_none()
            );
            assert!(
                work.plan_inspection_attempt(
                    root.path(),
                    &source,
                    &different,
                    "new-contract",
                    AttemptUpdate::Retry
                )
                .is_err()
            );
            let (plan, _) = work
                .plan_inspection_attempt(
                    root.path(),
                    &source,
                    &request,
                    "new-contract",
                    AttemptUpdate::Retry,
                )
                .unwrap();
            session.apply(plan).unwrap();
            assert!(
                work.load_inspection_hold(root.path(), &request, "new-contract")
                    .unwrap()
                    .is_none()
            );
            assert_eq!(work.progress(root.path(), &source).unwrap().completed, 0);
        }
    }

    #[test]
    fn original_timeouts_decode_without_rewriting_and_reject_ambiguous_state() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        let nodes = work.nodes().unwrap();
        let request = work
            .prepare_inspection(root.path(), &source)
            .unwrap()
            .1
            .unwrap();
        let id = request["node_id"].as_str().unwrap();
        let value = json!({"schema":"inspection-timeouts-v1", "work_id":work.plan.id,
            "entries":{id:{"request_revision":inspection::request_revision(&request,"contract").unwrap(),"count":2}}});
        let raw = value.to_string();
        let path = root.path().join(path(&work).unwrap());
        std::fs::write(&path, &raw).unwrap();
        assert!(
            work.load_inspection_hold(root.path(), &request, "contract")
                .unwrap()
                .is_some()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        let duplicate = raw.replacen("\"count\":2", "\"count\":1,\"count\":2", 1);
        assert!(InspectionAttempts::decode(&duplicate, &work, &nodes).is_err());
    }

    #[test]
    fn timeout_storage_covers_the_declared_tree_independently_of_model_input_limits() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut source = resources();
        source[0].text = "Source evidence. ".repeat(10000);
        let work = Work::create(&session, &source, "large", context("large"), limits()).unwrap();
        let nodes = work.nodes().unwrap();
        let timeouts = InspectionAttempts {
            schema: Schema::V2,
            work_id: work.plan.id.clone(),
            entries: nodes
                .iter()
                .map(|node| {
                    (
                        node.id.clone(),
                        Attempt::Timeout {
                            request_revision: "a".repeat(64),
                            count: MAX_TIMEOUTS,
                        },
                    )
                })
                .collect(),
        };
        let raw = encoded(&timeouts).unwrap();
        assert!(raw.len() > work.max_input_bytes);
        InspectionAttempts::decode(&raw, &work, &nodes).unwrap();
        let oversized = format!("{raw}{}", " ".repeat(storage_bound(&nodes).unwrap()));
        let error = InspectionAttempts::decode(&oversized, &work, &nodes)
            .err()
            .unwrap();
        assert!(error.to_string().contains("storage bound"));
    }

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
                .plan_inspection_attempt(
                    root.path(),
                    &source,
                    &request,
                    contract,
                    AttemptUpdate::TimedOut,
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
            work.plan_inspection_attempt(
                root.path(),
                &source,
                &request,
                contract,
                AttemptUpdate::TimedOut
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
            .plan_inspection_attempt(
                root.path(),
                &source,
                &request,
                contract,
                AttemptUpdate::Retry,
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
            work.plan_inspection_attempt(
                root.path(),
                &source,
                &request,
                contract,
                AttemptUpdate::Retry
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
            .plan_inspection_attempt(
                root.path(),
                &source,
                &request,
                "contract",
                AttemptUpdate::TimedOut,
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
                work.plan_inspection_attempt(
                    root.path(),
                    &source,
                    &request,
                    "contract",
                    AttemptUpdate::Retry
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), invalid);
        }
    }
}
