use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use lk_queue::QueueTask;
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::host::{HoldIdentity, HoldScope, HostAnswers, PendingHold};
use super::super::state::InvocationFailure;
use super::super::{Subject, Work};

const DIRECTORY: &str = ".lorekeeper/queue/generation-rejections";
const MAX_BYTES: usize = 65_536;
const MAX_REPLY_BYTES: usize = 16_384;
const PROPOSAL_SCHEMA: &str = "native-generation-rejection-v1";

/// Why Rust refused a decoded proposal for its current input.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands) enum ProposalRejection {
    UnknownOwnerReference,
    /// Publication guards refused the assembled result although its captured input was current.
    PublicationGuard {
        kind: crate::commands::queue::application::RejectionKind,
        detail: String,
    },
}

impl std::fmt::Display for ProposalRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOwnerReference => {
                f.write_str("native reuse reference is outside the admitted owner roster")
            }
            Self::PublicationGuard { detail, .. } => {
                write!(
                    f,
                    "publication guards refused the result for its current input: {detail}"
                )
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
enum Schema {
    #[serde(rename = "native-generation-rejection-v2")]
    V2,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: Schema,
    task_id: String,
    cache_hash: String,
    input_revision: String,
    input: Input,
    request_revision: String,
    cause: Cause,
}

/// Why an unchanged task input is held: Rust refused a decoded proposal, or the
/// tool-enabled host returned no reply.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands::work) enum Cause {
    Proposal {
        rejection: ProposalRejection,
        reply_revision: String,
        reply_bytes: usize,
        reply: Option<Value>,
    },
    Invocation {
        failure: InvocationFailure,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        exhausted: bool,
    },
}

impl Cause {
    pub(in crate::commands::work) fn invocation(failure: InvocationFailure) -> Self {
        Self::Invocation {
            failure,
            exhausted: false,
        }
    }

    pub(in crate::commands::work) fn proposal(
        reply: Value,
        rejection: ProposalRejection,
    ) -> miette::Result<Self> {
        let reply_bytes = serde_json::to_vec(&reply).into_diagnostic()?.len();
        Ok(Self::Proposal {
            rejection,
            reply_revision: super::digest(&reply)?,
            reply_bytes,
            reply: (reply_bytes <= MAX_REPLY_BYTES).then_some(reply),
        })
    }

    fn validate(&self) -> miette::Result<()> {
        match self {
            Self::Proposal {
                rejection,
                reply_revision,
                reply_bytes,
                reply,
            } => {
                if !is_revision(reply_revision, 64)
                    || *reply_bytes == 0
                    || matches!(rejection, ProposalRejection::PublicationGuard { detail, .. } if detail.trim().is_empty())
                {
                    return Err(miette::miette!(
                        "invalid native generation rejection identity"
                    ));
                }
                if let Some(reply) = reply {
                    if serde_json::to_vec(reply).into_diagnostic()?.len() != *reply_bytes
                        || *reply_bytes > MAX_REPLY_BYTES
                        || super::digest(reply)? != *reply_revision
                    {
                        return Err(miette::miette!(
                            "native generation rejection reply differs from its guard"
                        ));
                    }
                } else if *reply_bytes <= MAX_REPLY_BYTES {
                    return Err(miette::miette!(
                        "native generation rejection omitted its bounded reply"
                    ));
                }
                Ok(())
            }
            Self::Invocation { failure, .. } => failure.validate(),
        }
    }
}

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proposal { rejection, .. } => rejection.fmt(f),
            Self::Invocation { failure, .. } => failure.fmt(f),
        }
    }
}

/// The proposal-only receipt written before invocation causes existed; read, never written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalReceipt {
    schema: String,
    task_id: String,
    cache_hash: String,
    input_revision: String,
    input: Input,
    request_revision: String,
    rejection: ProposalRejection,
    reply_revision: String,
    reply_bytes: usize,
    reply: Option<Value>,
}

impl ProposalReceipt {
    fn into_receipt(self) -> miette::Result<Receipt> {
        if self.schema != PROPOSAL_SCHEMA {
            return Err(miette::miette!(
                "invalid native generation rejection schema"
            ));
        }
        Ok(Receipt {
            schema: Schema::V2,
            task_id: self.task_id,
            cache_hash: self.cache_hash,
            input_revision: self.input_revision,
            input: self.input,
            request_revision: self.request_revision,
            cause: Cause::Proposal {
                rejection: self.rejection,
                reply_revision: self.reply_revision,
                reply_bytes: self.reply_bytes,
                reply: self.reply,
            },
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands::work) enum Input {
    Selected,
    Work { work_id: String },
}

fn path(task: &QueueTask) -> PathBuf {
    identity_path(&task.task_id, &task.cache_hash)
}

fn identity_path(task_id: &str, cache_hash: &str) -> PathBuf {
    let identity = serde_json::to_vec(&(task_id, cache_hash)).expect("identity serializes");
    Path::new(DIRECTORY).join(format!("{}.json", blake3::hash(&identity).to_hex()))
}

fn is_revision(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read(root: &Path, path: &Path) -> miette::Result<Option<(String, Receipt)>> {
    let absolute = lk_vault::page_path(root, path).into_diagnostic()?;
    let file = match lk_core::fs::open_regular_file(&absolute) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    };
    let mut raw = String::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_string(&mut raw)
        .into_diagnostic()?;
    if raw.len() > MAX_BYTES {
        return Err(miette::miette!(
            "native generation rejection exceeds its storage bound"
        ));
    }
    let value = lk_core::json::decode_object(raw.as_bytes()).into_diagnostic()?;
    let receipt: Receipt = if value["schema"] == PROPOSAL_SCHEMA {
        serde_json::from_value::<ProposalReceipt>(value)
            .into_diagnostic()?
            .into_receipt()?
    } else {
        serde_json::from_value(value).into_diagnostic()?
    };
    if receipt.task_id.trim().is_empty()
        || !is_revision(&receipt.cache_hash, 32)
        || !is_revision(&receipt.input_revision, 64)
        || !is_revision(&receipt.request_revision, 64)
        || identity_path(&receipt.task_id, &receipt.cache_hash) != path
        || matches!(&receipt.input, Input::Work { work_id } if !is_revision(work_id, 64))
    {
        return Err(miette::miette!(
            "invalid native generation rejection identity"
        ));
    }
    receipt.cause.validate()?;
    Ok(Some((raw, receipt)))
}

pub(in crate::commands::work) fn held(
    root: &Path,
    task: &QueueTask,
    input_revision: &str,
) -> miette::Result<Option<String>> {
    let Some((_, receipt)) =
        read(root, &path(task))?.filter(|(_, receipt)| receipt.input_revision == input_revision)
    else {
        return Ok(None);
    };
    let retry = format!(
        "lore work retry-generation --task-id {} --input-revision {}",
        receipt.task_id, receipt.input_revision
    );
    Ok(match &receipt.cause {
        Cause::Invocation { failure, exhausted } => HostAnswers::load(root)?
            .standing(&identity(&receipt), failure, *exhausted)
            .reason(failure, &retry),
        Cause::Proposal { .. } => Some(format!(
            "{}; retained native generation input {}; use {retry} after diagnosis",
            receipt.cause, receipt.input_revision
        )),
    })
}

fn identity(receipt: &Receipt) -> HoldIdentity {
    HoldIdentity::Generation {
        task_id: receipt.task_id.clone(),
        cache_hash: receipt.cache_hash.clone(),
    }
}

/// Every pending unanswered generation hold, for choosing a probe.
pub(in crate::commands::work) fn pending_holds(root: &Path) -> miette::Result<Vec<PendingHold>> {
    let mut holds = Vec::new();
    for destination in super::super::host::store_entries(root, Path::new(DIRECTORY))? {
        let Some((_, receipt)) = read(root, &destination)? else {
            continue;
        };
        if let Cause::Invocation {
            failure,
            exhausted: false,
        } = &receipt.cause
            && failure.is_unanswered()
        {
            holds.push(PendingHold {
                identity: identity(&receipt),
                engine: Some(failure.engine()),
                scope: HoldScope::Queue {
                    task_id: receipt.task_id.clone(),
                    cache_hash: receipt.cache_hash.clone(),
                },
                host: failure.host().cloned(),
            });
        }
    }
    Ok(holds)
}

fn entries(root: &Path) -> miette::Result<Vec<PathBuf>> {
    let absolute = lk_vault::page_path(root, Path::new(DIRECTORY)).into_diagnostic()?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "native generation rejection store must be a direct directory"
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).into_diagnostic(),
    }
    std::fs::read_dir(absolute)
        .into_diagnostic()?
        .map(|entry| Ok(Path::new(DIRECTORY).join(entry.into_diagnostic()?.file_name())))
        .collect()
}

pub(in crate::commands::work) struct Binding<'a> {
    pub task: &'a QueueTask,
    pub work: &'a Work,
    pub input: Input,
    pub request: &'a Value,
}

pub(in crate::commands::work) fn plan_record(
    root: &Path,
    binding: Binding<'_>,
    cause: Cause,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<String> {
    let Binding {
        task,
        work,
        input,
        request,
    } = binding;
    let destination = path(task);
    let prior = read(root, &destination)?;
    let request_revision = super::digest(request)?;
    let mut cause = cause;
    if let Cause::Invocation {
        failure, exhausted, ..
    } = &mut cause
        && let Some((
            _,
            Receipt {
                input_revision,
                request_revision: prior_request,
                cause:
                    Cause::Invocation {
                        failure: prior_failure,
                        exhausted: prior_exhausted,
                    },
                ..
            },
        )) = &prior
        && *input_revision == work.plan.subject_revision
        && *prior_request == request_revision
    {
        *exhausted =
            HostAnswers::load(root)?.settle(failure, Some((prior_failure, *prior_exhausted)));
    }
    let before = prior.map(|(raw, _)| raw);
    let receipt = Receipt {
        schema: Schema::V2,
        task_id: task.task_id.clone(),
        cache_hash: task.cache_hash.clone(),
        input_revision: work.plan.subject_revision.clone(),
        input,
        request_revision,
        cause,
    };
    receipt.cause.validate()?;
    let raw = serde_json::to_string(&receipt).into_diagnostic()?;
    if raw.len() > MAX_BYTES {
        return Err(miette::miette!(
            "native generation rejection exceeds its storage bound"
        ));
    }
    let reason = format!(
        "{}; retained native generation input {}",
        receipt.cause, receipt.input_revision
    );
    plan.publish_expected(root, destination, before, raw)
        .into_diagnostic()?;
    Ok(reason)
}

pub(in crate::commands::work) fn plan_retry(
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task_id: &str,
    input_revision: &str,
) -> miette::Result<lk_vault::MutationPlan> {
    let task = crate::commands::queue::current_task(root, task_id)?;
    let destination = path(&task);
    let (raw, receipt) = read(root, &destination)?
        .ok_or_else(|| miette::miette!("no retained native generation rejection for this task"))?;
    let (work, prepared) = match &receipt.input {
        Input::Selected => {
            let mut prepared =
                super::super::task_for_plan(root, config, config_revision, task_id, None)?;
            let work = super::super::expected(root, &mut prepared, config, config_revision)?;
            (work, prepared)
        }
        Input::Work { work_id } => super::super::load(root, config, config_revision, work_id)?,
    };
    if receipt.input_revision != input_revision
        || work.plan.subject_revision != input_revision
        || !matches!(&prepared.subject, Subject::Queue { task_id, cache_hash, .. } if *task_id == task.task_id && *cache_hash == task.cache_hash)
    {
        return Err(miette::miette!(
            "native generation retry differs from the current input revision"
        ));
    }
    let mut plan = lk_vault::MutationPlan::default();
    prepared.guard_pages(&mut plan);
    plan.remove(destination, raw);
    Ok(plan)
}

pub(super) fn plan_prune(
    root: &Path,
    pending: &BTreeSet<(String, String)>,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<usize> {
    let mut removed = 0;
    for destination in entries(root)? {
        let (raw, receipt) = read(root, &destination)?.ok_or_else(|| {
            miette::miette!("native generation rejection disappeared during retention")
        })?;
        if pending.contains(&(receipt.task_id, receipt.cache_hash)) {
            plan.require_unchanged(destination, raw);
        } else {
            plan.remove(destination, raw);
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, QueueTask, PathBuf, Value) {
        let directory = tempfile::tempdir().unwrap();
        let task: QueueTask = serde_json::from_value(json!({
            "task_id":"task", "created_at":"2026-10-24T00:00:00Z", "kind":"extract-concepts", "cache_hash":"a".repeat(32),
            "target":{"vault_path":"wiki/documents/source.md","kind":"document-concepts","anchor":"## Related Concepts"},"input":{}
        })).unwrap();
        let reply = json!({"status":"complete","value":{"concepts":[]},"reason":null});
        let receipt = json!({
            "schema":PROPOSAL_SCHEMA, "task_id":task.task_id, "cache_hash":task.cache_hash,
            "input":{"kind":"selected"}, "input_revision":"b".repeat(64), "request_revision":"c".repeat(64),
            "reply_revision":super::super::digest(&reply).unwrap(),
            "reply_bytes":serde_json::to_vec(&reply).unwrap().len(), "reply":reply,
            "rejection":{"reason":"unknown-owner-reference"},
        });
        let destination = path(&task);
        std::fs::create_dir_all(directory.path().join(destination.parent().unwrap())).unwrap();
        (directory, task, destination, receipt)
    }

    #[test]
    fn current_input_holds_ignore_contract_changes_and_retention_protects_pending_tasks() {
        let (directory, task, destination, mut receipt) = fixture();
        let root = directory.path();
        for contract in ["c", "d"] {
            receipt["request_revision"] = json!(contract.repeat(64));
            let raw = serde_json::to_vec(&receipt).unwrap();
            std::fs::write(root.join(&destination), &raw).unwrap();
            assert!(held(root, &task, &"b".repeat(64)).unwrap().is_some());
            assert!(held(root, &task, &"e".repeat(64)).unwrap().is_none());
            assert_eq!(std::fs::read(root.join(&destination)).unwrap(), raw);
        }
        let session = lk_vault::MutationSession::acquire(root).unwrap();
        let mut keep = lk_vault::MutationPlan::default();
        assert_eq!(
            plan_prune(
                root,
                &BTreeSet::from([(task.task_id, task.cache_hash)]),
                &mut keep
            )
            .unwrap(),
            0
        );
        session.apply(keep).unwrap();
        assert!(root.join(&destination).exists());
        let mut prune = lk_vault::MutationPlan::default();
        assert_eq!(plan_prune(root, &BTreeSet::new(), &mut prune).unwrap(), 1);
        session.apply(prune).unwrap();
        assert!(!root.join(destination).exists());
    }

    #[test]
    fn invocation_failures_hold_their_input_only_in_strict_receipts() {
        let (directory, task, destination, proposal) = fixture();
        let root = directory.path();
        let mut receipt = json!({
            "schema":"native-generation-rejection-v2", "task_id":task.task_id, "cache_hash":task.cache_hash,
            "input":{"kind":"work","work_id":"d".repeat(64)}, "input_revision":"b".repeat(64),
            "request_revision":"c".repeat(64),
            "cause":{"kind":"invocation","failure":{"kind":"exited","engine":"codex","code":7}},
        });
        std::fs::write(
            root.join(&destination),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let reason = held(root, &task, &"b".repeat(64)).unwrap().unwrap();
        assert!(reason.contains("exited with status 7"), "{reason}");
        assert!(reason.contains("retried automatically"), "{reason}");
        assert!(held(root, &task, &"e".repeat(64)).unwrap().is_none());
        crate::commands::work::host::record_answer(
            root,
            lk_core::execution::AgentEngine::Codex,
            "codex",
            jiff::Timestamp::now(),
        )
        .unwrap();
        assert!(
            held(root, &task, &"b".repeat(64)).unwrap().is_none(),
            "a hold recorded without its failure time is released by any later answer"
        );
        receipt["cause"]["exhausted"] = json!(true);
        std::fs::write(
            root.join(&destination),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let reason = held(root, &task, &"b".repeat(64)).unwrap().unwrap();
        assert!(reason.contains("again after the host answered"), "{reason}");
        let mut guarded = json!({"kind":"proposal","reply_revision":proposal["reply_revision"],
            "reply_bytes":proposal["reply_bytes"],"reply":proposal["reply"],
            "rejection":{"reason":"publication-guard","kind":"evidence-scope-changed","detail":"owner left the captured scope"}});
        receipt["cause"] = guarded.clone();
        std::fs::write(
            root.join(&destination),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let reason = held(root, &task, &"b".repeat(64)).unwrap().unwrap();
        assert!(reason.contains("owner left the captured scope"), "{reason}");
        guarded["rejection"]["detail"] = json!(" ");
        for cause in [
            json!({"kind":"invocation","failure":{"kind":"exited","engine":"codex","code":0}}),
            json!({"kind":"proposal","rejection":proposal["rejection"]}),
            guarded,
            json!({"kind":"unknown"}),
        ] {
            receipt["cause"] = cause;
            std::fs::write(
                root.join(&destination),
                serde_json::to_vec(&receipt).unwrap(),
            )
            .unwrap();
            assert!(held(root, &task, &"b".repeat(64)).is_err(), "{receipt}");
        }
        let mut layout = proposal;
        layout["schema"] = json!("native-generation-rejection-v2");
        std::fs::write(
            root.join(&destination),
            serde_json::to_vec(&layout).unwrap(),
        )
        .unwrap();
        assert!(held(root, &task, &"b".repeat(64)).is_err());
    }

    #[test]
    fn invalid_or_ambiguous_receipts_never_become_an_unheld_input() {
        let (directory, task, destination, receipt) = fixture();
        let root = directory.path();
        for (field, value) in [
            ("schema", json!("unknown")),
            ("input_revision", json!("unknown")),
            ("task_id", json!("another-task")),
            ("rejection", json!({"reason":"unknown"})),
            ("reply", Value::Null),
            ("reply_bytes", json!(0)),
            ("reply_revision", json!("f".repeat(64))),
        ] {
            let mut invalid = receipt.clone();
            invalid[field] = value;
            std::fs::write(
                root.join(&destination),
                serde_json::to_vec(&invalid).unwrap(),
            )
            .unwrap();
            assert!(held(root, &task, &"b".repeat(64)).is_err(), "{field}");
        }
        let raw = serde_json::to_string(&receipt).unwrap().replacen(
            "\"concepts\":[]",
            "\"concepts\":[],\"concepts\":[]",
            1,
        );
        std::fs::write(root.join(&destination), raw).unwrap();
        assert!(held(root, &task, &"b".repeat(64)).is_err());
        std::fs::write(root.join(&destination), " ".repeat(MAX_BYTES + 1)).unwrap();
        assert!(held(root, &task, &"b".repeat(64)).is_err());
    }
}
