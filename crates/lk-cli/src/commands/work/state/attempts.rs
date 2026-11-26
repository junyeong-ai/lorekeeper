use super::*;
#[cfg(unix)]
use crate::commands::work::host::PendingHold;
use crate::commands::work::host::{HoldIdentity, HostAnswers};

/// The most session timeouts a record written before timeouts became unanswered failures
/// could count.
const LEGACY_TIMEOUT_LIMIT: usize = 2;
const MAX_HOLD_REASON_BYTES: usize = 256;

fn storage_bound(nodes: &[Node]) -> miette::Result<usize> {
    nodes
        .len()
        .checked_add(1)
        .and_then(|count| count.checked_mul(1024))
        .ok_or_else(|| fail("inspection attempt storage bound overflow"))
}

/// How a host invocation failed, and when the host finished it. Failures recorded before
/// that time was kept have none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum InvocationFailure {
    TimedOut {
        engine: lk_core::execution::AgentEngine,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_at: Option<jiff::Timestamp>,
    },
    Exited {
        engine: lk_core::execution::AgentEngine,
        code: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_at: Option<jiff::Timestamp>,
    },
    InvalidResponse {
        engine: lk_core::execution::AgentEngine,
        revision: String,
        bytes: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_at: Option<jiff::Timestamp>,
    },
    MissingResponse {
        engine: lk_core::execution::AgentEngine,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failed_at: Option<jiff::Timestamp>,
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
    /// one.
    pub(crate) fn is_unanswered(&self) -> bool {
        !matches!(self, Self::InvalidResponse { .. })
    }

    pub(crate) fn engine(&self) -> lk_core::execution::AgentEngine {
        match self {
            Self::TimedOut { engine, .. }
            | Self::Exited { engine, .. }
            | Self::InvalidResponse { engine, .. }
            | Self::MissingResponse { engine, .. } => *engine,
        }
    }

    /// When the host finished the failed invocation, if that was recorded.
    pub(crate) fn failed_at(&self) -> Option<jiff::Timestamp> {
        match self {
            Self::TimedOut { failed_at, .. }
            | Self::Exited { failed_at, .. }
            | Self::InvalidResponse { failed_at, .. }
            | Self::MissingResponse { failed_at, .. } => *failed_at,
        }
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
    /// Session timeouts counted before a timeout became an unanswered failure; read, never
    /// written. Its engine and time were not kept, so any recorded answer releases it.
    Timeout {
        request_revision: String,
        count: usize,
    },
    Failed {
        canonical_revision: String,
        request_revision: String,
        failure: InvocationFailure,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        exhausted: bool,
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
                    if *count == 0 || *count > LEGACY_TIMEOUT_LIMIT {
                        return Err(fail("invalid inspection timeout count"));
                    }
                }
                Attempt::Failed {
                    canonical_revision,
                    request_revision,
                    failure,
                    ..
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

    pub(super) fn reason(
        &self,
        request: &Value,
        contract: &str,
        answers: &HostAnswers,
    ) -> miette::Result<Option<String>> {
        let id = request["node_id"]
            .as_str()
            .ok_or_else(|| fail("inspection has no node"))?;
        let Some(entry) = self.entries.get(id) else {
            return Ok(None);
        };
        let retry = format!("lore work retry {} {id}", self.work_id);
        if let Attempt::Failed {
            canonical_revision,
            failure,
            exhausted,
            ..
        } = entry
        {
            if *canonical_revision != revision(request)? {
                return Ok(None);
            }
            let identity = HoldIdentity::Inspection {
                work_id: self.work_id.clone(),
                node_id: id.into(),
            };
            return Ok(answers
                .standing(&identity, failure, *exhausted)
                .reason(failure, &retry));
        }
        Ok(match entry {
            Attempt::Timeout {
                request_revision, ..
            } if *request_revision == inspection::request_revision(request, contract)?
                && !answers.has_answer() =>
            {
                Some(format!(
                    "native invocation reached its session timeout; retried automatically once a host answers another request; {retry} retries it now"
                ))
            }
            Attempt::Held {
                canonical_revision,
                reason,
            } if *canonical_revision == revision(request)? => {
                Some(format!("{reason}; diagnose the host, then use {retry}"))
            }
            _ => None,
        })
    }
}

/// Every pending unanswered inspection hold of a queue Work, for choosing a probe.
#[cfg(unix)]
pub(in crate::commands) fn pending_holds(
    root: &Path,
) -> miette::Result<Vec<(lk_core::execution::AgentEngine, PendingHold)>> {
    let mut holds = Vec::new();
    for directory in
        crate::commands::work::host::store_entries(root, Path::new(".lorekeeper/work"))?
    {
        let Some(work_id) = directory
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| require_id(name).is_ok())
        else {
            continue;
        };
        let Some(raw) = read_optional(root, &work_path(work_id, "inspection-timeouts.json")?)?
        else {
            continue;
        };
        let StoredAttempts::Attempts { entries, .. } =
            super::super::receipt::decode(&raw).map_err(fail)?
        else {
            continue;
        };
        let pending = entries
            .into_iter()
            .filter_map(|(node_id, entry)| match entry {
                Attempt::Failed {
                    failure,
                    exhausted: false,
                    ..
                } if failure.is_unanswered() => {
                    Some((node_id, failure.engine(), failure.failed_at()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            continue;
        }
        let Some(plan) = read_optional(root, &work_path(work_id, "plan.json")?)? else {
            continue;
        };
        let plan = lk_core::json::decode_object(plan.as_bytes()).map_err(fail)?;
        let subject = &plan["context"]["subject"];
        let (Some("queue"), Some(task_id), Some(cache_hash)) = (
            subject["kind"].as_str(),
            subject["task_id"].as_str(),
            subject["cache_hash"].as_str(),
        ) else {
            continue;
        };
        for (node_id, engine, failed_at) in pending {
            holds.push((
                engine,
                PendingHold {
                    identity: HoldIdentity::Inspection {
                        work_id: work_id.into(),
                        node_id,
                    },
                    task: (task_id.into(), cache_hash.into()),
                    failed_at,
                },
            ));
        }
    }
    Ok(holds)
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
    /// Plans one operational attempt record for an exact ready inspection and returns the hold
    /// reason it leaves, if any.
    #[cfg(unix)]
    pub fn plan_inspection_attempt(
        &self,
        root: &Path,
        resources: &[SemanticResource],
        request: &Value,
        contract: &str,
        update: AttemptUpdate,
    ) -> miette::Result<(MutationPlan, Option<String>)> {
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
        let answers = HostAnswers::load(root)?;
        let current = attempts.entries.get(&node.id);
        let held = match update {
            AttemptUpdate::Retry => {
                let retryable = match current {
                    Some(Attempt::Timeout {
                        request_revision: previous,
                        ..
                    }) => previous == &request_revision,
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
                    return Err(fail("inspection has no current failed invocation or hold"));
                }
                attempts.entries.remove(&node.id);
                None
            }
            AttemptUpdate::Failed(failure) => {
                failure.validate()?;
                if attempts.reason(request, contract, &answers)?.is_some() {
                    return Err(fail("inspection already has a current operational hold"));
                }
                let prior = match current {
                    Some(Attempt::Failed {
                        canonical_revision: previous,
                        failure,
                        exhausted,
                        ..
                    }) if *previous == canonical_revision => Some((failure.clone(), *exhausted)),
                    Some(Attempt::Timeout {
                        request_revision: previous,
                        ..
                    }) if *previous == request_revision => Some((
                        InvocationFailure::TimedOut {
                            engine: failure.engine(),
                            failed_at: None,
                        },
                        false,
                    )),
                    _ => None,
                };
                let exhausted = answers.exhausts(
                    &failure,
                    prior
                        .as_ref()
                        .map(|(failure, exhausted)| (failure, *exhausted)),
                );
                attempts.entries.insert(
                    node.id.clone(),
                    Attempt::Failed {
                        canonical_revision,
                        request_revision,
                        failure,
                        exhausted,
                    },
                );
                attempts.reason(request, contract, &answers)?
            }
            AttemptUpdate::Hold(reason) => {
                validate_reason(&reason)?;
                if attempts.reason(request, contract, &answers)?.is_some() {
                    return Err(fail("inspection already has a current operational hold"));
                }
                attempts.entries.insert(
                    node.id.clone(),
                    Attempt::Held {
                        canonical_revision,
                        reason,
                    },
                );
                attempts.reason(request, contract, &answers)?
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
        Ok((mutation, held))
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
                failed_at: Some(jiff::Timestamp::now()),
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
            let (plan, held) = work
                .plan_inspection_attempt(root.path(), &source, &request, "old-contract", update)
                .unwrap();
            assert!(held.is_some());
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
    fn an_unanswered_failure_is_retried_once_after_its_host_answers() {
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
        let fail_again = || {
            let (plan, _) = work
                .plan_inspection_attempt(
                    root.path(),
                    &source,
                    &request,
                    "contract",
                    AttemptUpdate::Failed(InvocationFailure::Exited {
                        engine: lk_core::execution::AgentEngine::Codex,
                        code: 1,
                        failed_at: Some(jiff::Timestamp::now()),
                    }),
                )
                .unwrap();
            session.apply(plan).unwrap();
        };
        let hold = || {
            work.load_inspection_hold(root.path(), &request, "contract")
                .unwrap()
        };
        let answer = || {
            std::thread::sleep(std::time::Duration::from_millis(2));
            crate::commands::work::host::record_answer(
                root.path(),
                lk_core::execution::AgentEngine::Codex,
                jiff::Timestamp::now(),
            )
            .unwrap();
        };
        fail_again();
        assert!(
            hold()
                .unwrap()
                .contains("retried automatically once the codex host answers")
        );
        crate::commands::work::host::record_answer(
            root.path(),
            lk_core::execution::AgentEngine::ClaudeCode,
            jiff::Timestamp::now(),
        )
        .unwrap();
        assert!(hold().is_some(), "another engine's answer proves nothing");
        answer();
        assert_eq!(hold(), None);
        fail_again();
        let exhausted = hold().unwrap();
        assert!(exhausted.contains("again after the host answered; diagnose the request"));
        answer();
        assert_eq!(hold(), Some(exhausted));
        let (plan, _) = work
            .plan_inspection_attempt(
                root.path(),
                &source,
                &request,
                "contract",
                AttemptUpdate::Retry,
            )
            .unwrap();
        session.apply(plan).unwrap();
        assert_eq!(hold(), None);
        fail_again();
        assert!(
            hold().unwrap().contains("retried automatically"),
            "an explicit retry starts a fresh judgment"
        );
    }

    #[test]
    fn legacy_timeouts_are_pending_until_any_answer_and_a_repeat_exhausts() {
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
        let hold = || {
            work.load_inspection_hold(root.path(), &request, "contract")
                .unwrap()
        };
        assert!(
            hold()
                .unwrap()
                .contains("retried automatically once a host answers"),
            "an exhausted legacy allowance is pending, not exhausted"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        let duplicate = raw.replacen("\"count\":2", "\"count\":1,\"count\":2", 1);
        assert!(InspectionAttempts::decode(&duplicate, &work, &nodes).is_err());
        crate::commands::work::host::record_answer(
            root.path(),
            lk_core::execution::AgentEngine::Codex,
            jiff::Timestamp::now(),
        )
        .unwrap();
        assert_eq!(hold(), None);
        let (plan, held) = work
            .plan_inspection_attempt(
                root.path(),
                &source,
                &request,
                "contract",
                AttemptUpdate::Failed(InvocationFailure::TimedOut {
                    engine: lk_core::execution::AgentEngine::Codex,
                    failed_at: Some(jiff::Timestamp::now()),
                }),
            )
            .unwrap();
        session.apply(plan).unwrap();
        assert!(
            held.unwrap().contains("again after the host answered"),
            "a timeout after the release exhausts rather than resetting an allowance"
        );
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
                            count: LEGACY_TIMEOUT_LIMIT,
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
    fn timeouts_hold_their_exact_input_and_retry_preserves_semantic_holds() {
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
        let timed_out = || {
            AttemptUpdate::Failed(InvocationFailure::TimedOut {
                engine: lk_core::execution::AgentEngine::Codex,
                failed_at: Some(jiff::Timestamp::now()),
            })
        };
        let (plan, held) = work
            .plan_inspection_attempt(root.path(), &source, &request, contract, timed_out())
            .unwrap();
        assert!(held.unwrap().contains("session timeout"));
        session.apply(plan).unwrap();
        assert!(
            work.plan_inspection_attempt(root.path(), &source, &request, contract, timed_out())
                .is_err()
        );
        assert!(
            work.load_inspection_hold(root.path(), &request, "different-contract")
                .unwrap()
                .is_some(),
            "the hold follows the exact input across contract changes"
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
                AttemptUpdate::Failed(InvocationFailure::TimedOut {
                    engine: lk_core::execution::AgentEngine::Codex,
                    failed_at: Some(jiff::Timestamp::now()),
                }),
            )
            .unwrap();
        session.apply(plan).unwrap();
        let path = root.path().join(path(&work).unwrap());
        let raw = std::fs::read(&path).unwrap();
        for field in ["schema", "work_id", "failure", "node"] {
            let mut value: Value = serde_json::from_slice(&raw).unwrap();
            let node = request["node_id"].as_str().unwrap();
            match field {
                "schema" => value["schema"] = json!("unknown"),
                "work_id" => value["work_id"] = json!("f".repeat(64)),
                "failure" => value["entries"][node]["failure"]["kind"] = json!("unknown"),
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
