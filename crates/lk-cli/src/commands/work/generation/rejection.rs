use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use lk_queue::QueueTask;
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::{Subject, Work};
use super::owners::ProposalRejection;

const DIRECTORY: &str = ".lorekeeper/queue/generation-rejections";
const MAX_BYTES: usize = 65_536;
const MAX_REPLY_BYTES: usize = 16_384;

#[derive(Serialize, Deserialize)]
enum Schema {
    #[serde(rename = "native-generation-rejection-v1")]
    V1,
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
    rejection: ProposalRejection,
    reply_revision: String,
    reply_bytes: usize,
    reply: Option<Value>,
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
    let value = crate::commands::json::decode_object(raw.as_bytes()).into_diagnostic()?;
    let receipt: Receipt = serde_json::from_value(value).into_diagnostic()?;
    let revision = |value: &str, length| {
        value.len() == length
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if receipt.task_id.trim().is_empty()
        || !revision(&receipt.cache_hash, 32)
        || !revision(&receipt.input_revision, 64)
        || !revision(&receipt.request_revision, 64)
        || !revision(&receipt.reply_revision, 64)
        || identity_path(&receipt.task_id, &receipt.cache_hash) != path
        || receipt.reply_bytes == 0
        || matches!(&receipt.input, Input::Work { work_id } if !revision(work_id, 64))
    {
        return Err(miette::miette!(
            "invalid native generation rejection identity"
        ));
    }
    if let Some(reply) = &receipt.reply {
        if serde_json::to_vec(reply).into_diagnostic()?.len() != receipt.reply_bytes
            || receipt.reply_bytes > MAX_REPLY_BYTES
            || super::digest(reply)? != receipt.reply_revision
        {
            return Err(miette::miette!(
                "native generation rejection reply differs from its guard"
            ));
        }
    } else if receipt.reply_bytes <= MAX_REPLY_BYTES {
        return Err(miette::miette!(
            "native generation rejection omitted its bounded reply"
        ));
    }
    Ok(Some((raw, receipt)))
}

pub(in crate::commands::work) fn held(
    root: &Path,
    task: &QueueTask,
    input_revision: &str,
) -> miette::Result<Option<String>> {
    Ok(read(root, &path(task))?.filter(|(_, receipt)| receipt.input_revision == input_revision).map(|(_, receipt)| {
        format!("{}; retained native generation input {}; use work retry-generation with this exact task and input revision", receipt.rejection, receipt.input_revision)
    }))
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
    reply: Value,
    rejection: ProposalRejection,
    plan: &mut lk_vault::MutationPlan,
) -> miette::Result<String> {
    let Binding {
        task,
        work,
        input,
        request,
    } = binding;
    let destination = path(task);
    let before = read(root, &destination)?.map(|(raw, _)| raw);
    let reply_bytes = serde_json::to_vec(&reply).into_diagnostic()?.len();
    let receipt = Receipt {
        schema: Schema::V1,
        task_id: task.task_id.clone(),
        cache_hash: task.cache_hash.clone(),
        input_revision: work.plan.subject_revision.clone(),
        input,
        request_revision: super::digest(request)?,
        reply_revision: super::digest(&reply)?,
        reply_bytes,
        reply: (reply_bytes <= MAX_REPLY_BYTES).then_some(reply),
        rejection,
    };
    let raw = serde_json::to_string(&receipt).into_diagnostic()?;
    if raw.len() > MAX_BYTES {
        return Err(miette::miette!(
            "native generation rejection exceeds its storage bound"
        ));
    }
    let reason = format!(
        "{}; retained native generation input {}",
        receipt.rejection, receipt.input_revision
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
            let prepared =
                super::super::task_for_plan(root, config, config_revision, task_id, None)?;
            let work = super::super::expected(root, &prepared, config, config_revision)?;
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
    let absolute = lk_vault::page_path(root, Path::new(DIRECTORY)).into_diagnostic()?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "native generation rejection store must be a direct directory"
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).into_diagnostic(),
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(absolute).into_diagnostic()? {
        let entry = entry.into_diagnostic()?;
        let destination = Path::new(DIRECTORY).join(entry.file_name());
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
        let receipt = serde_json::to_value(Receipt {
            schema: Schema::V1,
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            input: Input::Selected,
            input_revision: "b".repeat(64),
            request_revision: "c".repeat(64),
            reply_revision: super::super::digest(&reply).unwrap(),
            reply_bytes: serde_json::to_vec(&reply).unwrap().len(),
            reply: Some(reply),
            rejection: ProposalRejection::UnknownOwnerReference,
        })
        .unwrap();
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
