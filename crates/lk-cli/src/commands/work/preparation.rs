use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use lk_vault::{MutationPlan, MutationSession};
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use super::host::{HoldIdentity, HoldScope, HostAnswers, PendingHold};

const QUEUE_DIRECTORY: &str = ".lorekeeper/queue/preparation-attempts";
const AUDIT_DIRECTORY: &str = ".lorekeeper/audit-preparation-attempts";
const SCHEMA: &str = "native-preparation-attempt-v1";
const MAX_BYTES: usize = 16_384;

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Stage {
    ExtractionDiscovery,
    WholeSection,
    AuditDiscovery,
    AuditScreening,
    AuditAssessment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands) enum Key {
    ExtractionDiscovery { task_id: String, cache_hash: String },
    WholeSection { task_id: String, cache_hash: String },
    AuditDiscovery { path: String },
    AuditScreening { path: String },
    AuditAssessment { path: String },
}

impl Key {
    fn task(&self) -> Option<(&str, &str)> {
        match self {
            Self::ExtractionDiscovery {
                task_id,
                cache_hash,
            }
            | Self::WholeSection {
                task_id,
                cache_hash,
            } => Some((task_id, cache_hash)),
            Self::AuditDiscovery { .. }
            | Self::AuditScreening { .. }
            | Self::AuditAssessment { .. } => None,
        }
    }

    fn path(&self) -> PathBuf {
        let directory = if self.task().is_some() {
            QUEUE_DIRECTORY
        } else {
            AUDIT_DIRECTORY
        };
        let bytes = serde_json::to_vec(self).expect("preparation key serializes");
        Path::new(directory).join(format!("{}.json", lk_core::rss_original::digest(&bytes)))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands) enum Disposition {
    InvocationFailed {
        failure: super::state::InvocationFailure,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        exhausted: bool,
    },
    NeedsAction {
        reason: String,
    },
}

impl Disposition {
    #[cfg(unix)]
    pub(in crate::commands) fn invocation_failed(failure: super::state::InvocationFailure) -> Self {
        Self::InvocationFailed {
            failure,
            exhausted: false,
        }
    }

    fn validate(&self) -> miette::Result<()> {
        match self {
            Self::InvocationFailed { failure, .. } => failure.validate(),
            Self::NeedsAction { reason } => {
                if reason.trim().is_empty()
                    || serde_json::to_vec(reason).into_diagnostic()?.len() > 256
                {
                    return Err(miette::miette!(
                        "preparation action requires a nonblank reason within 256 compact JSON bytes"
                    ));
                }
                Ok(())
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    key: Key,
    subject: String,
    subject_revision: String,
    input_revision: String,
    contract_revision: String,
    disposition: Disposition,
}

#[cfg(unix)]
impl Receipt {
    fn host_identity(&self) -> HoldIdentity {
        HoldIdentity::Preparation {
            key: self.key.clone(),
        }
    }

    fn retry_command(&self) -> String {
        let scope = match &self.key {
            Key::ExtractionDiscovery { task_id, .. } | Key::WholeSection { task_id, .. } => {
                format!("--task-id {task_id}")
            }
            Key::AuditDiscovery { path }
            | Key::AuditScreening { path }
            | Key::AuditAssessment { path } => format!("--path {path}"),
        };
        let stage = serde_json::to_value(&self.key).expect("preparation key serializes")["stage"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        format!(
            "lore work retry-preparation --stage {stage} {scope} --input-revision {}",
            self.input_revision
        )
    }
}

/// Every pending unanswered preparation hold, for choosing a probe.
#[cfg(unix)]
pub(in crate::commands) fn pending_holds(root: &Path) -> miette::Result<Vec<PendingHold>> {
    let mut holds = Vec::new();
    for directory in [QUEUE_DIRECTORY, AUDIT_DIRECTORY] {
        for path in super::host::store_entries(root, Path::new(directory))? {
            let Some((_, receipt)) = read(root, &path)? else {
                continue;
            };
            let Disposition::InvocationFailed {
                failure,
                exhausted: false,
            } = &receipt.disposition
            else {
                continue;
            };
            if !failure.is_unanswered() {
                continue;
            }
            let scope = match receipt.key.task() {
                Some((task_id, cache_hash)) => HoldScope::Queue {
                    task_id: task_id.into(),
                    cache_hash: cache_hash.into(),
                },
                None => HoldScope::Audit {
                    path: receipt.subject.clone(),
                    subject_revision: receipt.subject_revision.clone(),
                },
            };
            holds.push(PendingHold {
                identity: receipt.host_identity(),
                engine: failure.engine(),
                scope,
                failed_at: failure.failed_at(),
            });
        }
    }
    Ok(holds)
}

fn revision(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read(root: &Path, path: &Path) -> miette::Result<Option<(String, Receipt)>> {
    let absolute = lk_vault::page_path(root, path).into_diagnostic()?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "preparation attempt must be a direct regular file"
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).into_diagnostic(),
    }
    let mut raw = String::new();
    std::fs::File::open(absolute)
        .into_diagnostic()?
        .take((MAX_BYTES + 1) as u64)
        .read_to_string(&mut raw)
        .into_diagnostic()?;
    if raw.len() > MAX_BYTES {
        return Err(miette::miette!(
            "preparation attempt exceeds its storage bound"
        ));
    }
    let value = lk_core::json::decode_object(raw.as_bytes())
        .map_err(|_| miette::miette!("preparation attempt requires unique JSON fields"))?;
    let receipt: Receipt = serde_json::from_value(value)
        .map_err(|_| miette::miette!("invalid preparation attempt fields"))?;
    if receipt.schema != SCHEMA
        || receipt.key.path() != path
        || !revision(&receipt.subject_revision, 64)
        || !revision(&receipt.input_revision, 64)
        || !revision(&receipt.contract_revision, 64)
    {
        return Err(miette::miette!(
            "invalid preparation attempt identity or scope"
        ));
    }
    lk_vault::page_path(root, Path::new(&receipt.subject)).into_diagnostic()?;
    if let Some((task, cache)) = receipt.key.task() {
        if task.trim().is_empty() || !revision(cache, 32) {
            return Err(miette::miette!("invalid preparation task identity"));
        }
    } else {
        let (Key::AuditDiscovery { path }
        | Key::AuditScreening { path }
        | Key::AuditAssessment { path }) = &receipt.key
        else {
            unreachable!()
        };
        if path != &receipt.subject {
            return Err(miette::miette!(
                "preparation attempt subject differs from its key"
            ));
        }
    }
    receipt.disposition.validate()?;
    Ok(Some((raw, receipt)))
}

#[cfg(unix)]
pub(in crate::commands) struct Binding {
    key: Key,
    subject: String,
    subject_revision: String,
    input_revision: String,
    contract_revision: String,
}

#[cfg(unix)]
impl Binding {
    pub fn extraction_discovery(
        task: &lk_queue::QueueTask,
        subject_revision: &str,
        request: &serde_json::Value,
        schema: &serde_json::Value,
    ) -> miette::Result<Self> {
        Self::build(
            Key::ExtractionDiscovery {
                task_id: task.task_id.clone(),
                cache_hash: task.cache_hash.clone(),
            },
            task.target.vault_path.clone(),
            subject_revision.into(),
            &serde_json::json!({"task":task,"subject_revision":subject_revision}),
            request,
            schema,
        )
    }

    pub fn audit_discovery(
        path: &str,
        subject_revision: &str,
        request: &serde_json::Value,
        schema: &serde_json::Value,
    ) -> miette::Result<Self> {
        Self::build(
            Key::AuditDiscovery { path: path.into() },
            path.into(),
            subject_revision.into(),
            &serde_json::json!({"path":path,"subject_revision":subject_revision}),
            request,
            schema,
        )
    }

    pub fn audit_screening(
        request: &lk_graph::audit::selection::ScreeningRequest,
        native: &serde_json::Value,
        schema: &serde_json::Value,
    ) -> miette::Result<Self> {
        Self::build(
            Key::AuditScreening {
                path: request.subject.clone(),
            },
            request.subject.clone(),
            request.subject_file_revision.clone(),
            &serde_json::to_value(request).into_diagnostic()?,
            native,
            schema,
        )
    }

    /// A tool-enabled whole or bounded assessment of one audit subject, bound to its
    /// semantic context so that assessment-rule changes alone keep the hold.
    pub fn audit_assessment(
        path: &str,
        subject_revision: &str,
        context_revision: &str,
        request: &serde_json::Value,
        schema: &serde_json::Value,
    ) -> miette::Result<Self> {
        Self::build(
            Key::AuditAssessment { path: path.into() },
            path.into(),
            subject_revision.into(),
            &serde_json::json!({"path":path,"context_revision":context_revision}),
            request,
            schema,
        )
    }

    pub fn build(
        key: Key,
        subject: String,
        subject_revision: String,
        input: &serde_json::Value,
        request: &serde_json::Value,
        schema: &serde_json::Value,
    ) -> miette::Result<Self> {
        let digest = |value: &serde_json::Value| -> miette::Result<String> {
            Ok(lk_core::rss_original::digest(
                &serde_json::to_vec(value).into_diagnostic()?,
            ))
        };
        Ok(Self {
            key,
            subject,
            subject_revision,
            input_revision: digest(input)?,
            contract_revision: digest(&serde_json::json!({"request":request,"schema":schema}))?,
        })
    }

    /// The retained attempt for this exact input, if one was recorded.
    fn retained(&self, root: &Path) -> miette::Result<Option<(String, Receipt)>> {
        Ok(read(root, &self.key.path())?.filter(|(_, receipt)| {
            receipt.subject == self.subject
                && receipt.subject_revision == self.subject_revision
                && receipt.input_revision == self.input_revision
        }))
    }

    pub fn held(&self, root: &Path) -> miette::Result<Option<String>> {
        let Some((_, receipt)) = self.retained(root)? else {
            return Ok(None);
        };
        let retry = receipt.retry_command();
        let reason = match &receipt.disposition {
            Disposition::InvocationFailed { failure, exhausted } => {
                let Some(reason) = HostAnswers::load(root)?
                    .standing(&receipt.host_identity(), failure, *exhausted)
                    .reason(failure, &retry)
                else {
                    return Ok(None);
                };
                reason
            }
            Disposition::NeedsAction { reason } => {
                format!("{reason}; use {retry} after diagnosis")
            }
        };
        Ok(Some(format!(
            "{reason}; retained preparation input {}",
            receipt.input_revision
        )))
    }

    pub fn plan_record(
        &self,
        root: &Path,
        mut disposition: Disposition,
    ) -> miette::Result<MutationPlan> {
        if self.held(root)?.is_some() {
            return Err(miette::miette!("preparation already has a current hold"));
        }
        if let Disposition::InvocationFailed {
            failure, exhausted, ..
        } = &mut disposition
            && let Some((
                _,
                Receipt {
                    disposition:
                        Disposition::InvocationFailed {
                            failure: prior,
                            exhausted: prior_exhausted,
                        },
                    ..
                },
            )) = self.retained(root)?
        {
            *exhausted =
                HostAnswers::load(root)?.exhausts(failure, Some((&prior, prior_exhausted)));
        }
        let path = self.key.path();
        let before = read(root, &path)?.map(|(raw, _)| raw);
        let raw = serde_json::to_string(&Receipt {
            schema: SCHEMA.into(),
            key: self.key.clone(),
            subject: self.subject.clone(),
            subject_revision: self.subject_revision.clone(),
            input_revision: self.input_revision.clone(),
            contract_revision: self.contract_revision.clone(),
            disposition,
        })
        .into_diagnostic()?;
        if raw.len() > MAX_BYTES {
            return Err(miette::miette!(
                "preparation attempt exceeds its storage bound"
            ));
        }
        let value: Receipt = crate::commands::work::receipt::decode(&raw).into_diagnostic()?;
        value.disposition.validate()?;
        let mut plan = MutationPlan::default();
        plan.publish_expected(root, path, before, raw)
            .into_diagnostic()?;
        Ok(plan)
    }
}

#[cfg(unix)]
pub(in crate::commands) fn plan_retry(
    root: &Path,
    key: &Key,
    input_revision: &str,
) -> miette::Result<MutationPlan> {
    let path = key.path();
    let (raw, receipt) =
        read(root, &path)?.ok_or_else(|| miette::miette!("preparation has no retained attempt"))?;
    if receipt.input_revision != input_revision {
        return Err(miette::miette!(
            "preparation retry names a different retained input"
        ));
    }
    let subject = lk_vault::page_path(root, Path::new(&receipt.subject)).into_diagnostic()?;
    let bytes = std::fs::read_to_string(subject).into_diagnostic()?;
    if lk_core::concept::file_revision(&bytes) != receipt.subject_revision {
        return Err(miette::miette!(
            "preparation subject changed; its old hold does not authorize the current input"
        ));
    }
    let mut plan = MutationPlan::default();
    plan.require_unchanged(Path::new(&receipt.subject), bytes);
    plan.remove(path, raw);
    Ok(plan)
}

fn entries(root: &Path, directory: &str) -> miette::Result<Vec<(PathBuf, String, Receipt)>> {
    let absolute = lk_vault::page_path(root, Path::new(directory)).into_diagnostic()?;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "preparation attempt store must be a direct directory"
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).into_diagnostic(),
    }
    std::fs::read_dir(absolute)
        .into_diagnostic()?
        .map(|entry| {
            let entry = entry.into_diagnostic()?;
            let path = Path::new(directory).join(entry.file_name());
            let (raw, receipt) = read(root, &path)?
                .ok_or_else(|| miette::miette!("preparation attempt disappeared during capture"))?;
            Ok((path, raw, receipt))
        })
        .collect()
}

pub(in crate::commands) fn plan_prune_queue(
    root: &Path,
    pending: &BTreeSet<(String, String)>,
    plan: &mut MutationPlan,
) -> miette::Result<usize> {
    let mut removed = 0;
    for (path, raw, receipt) in entries(root, QUEUE_DIRECTORY)? {
        let (task, cache) = receipt
            .key
            .task()
            .ok_or_else(|| miette::miette!("audit preparation stored in queue attempts"))?;
        if !pending.contains(&(task.into(), cache.into())) {
            plan.remove(path, raw);
            removed += 1;
        } else {
            plan.require_unchanged(path, raw);
        }
    }
    Ok(removed)
}

pub(in crate::commands) fn audit_present(root: &Path) -> miette::Result<bool> {
    let absolute = lk_vault::page_path(root, Path::new(AUDIT_DIRECTORY)).into_diagnostic()?;
    match std::fs::symlink_metadata(absolute) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(miette::miette!(
            "audit preparation store must be a direct directory"
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).into_diagnostic(),
    }
}

pub(in crate::commands) fn prune_absent_audits(
    root: &Path,
    session: Option<&MutationSession>,
) -> miette::Result<usize> {
    let mut plan = MutationPlan::default();
    let mut removed = 0;
    for (path, raw, receipt) in entries(root, AUDIT_DIRECTORY)? {
        let subject = lk_vault::page_path(root, Path::new(&receipt.subject)).into_diagnostic()?;
        match std::fs::symlink_metadata(subject) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                plan.require_absent(Path::new(&receipt.subject));
                plan.remove(path, raw);
                removed += 1;
            }
            Err(error) => return Err(error).into_diagnostic(),
        }
    }
    if let Some(session) = session
        && removed > 0
    {
        session.apply(plan).into_diagnostic()?;
    }
    Ok(removed)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preparation_attempts_bind_inputs_survive_contract_changes_and_follow_owner_retention() {
        for key in [
            Key::ExtractionDiscovery {
                task_id: "task".into(),
                cache_hash: "a".repeat(32),
            },
            Key::WholeSection {
                task_id: "task".into(),
                cache_hash: "a".repeat(32),
            },
            Key::AuditDiscovery {
                path: "wiki/documents/source.md".into(),
            },
            Key::AuditScreening {
                path: "wiki/documents/source.md".into(),
            },
            Key::AuditAssessment {
                path: "wiki/documents/source.md".into(),
            },
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
            let subject = "wiki/documents/source.md";
            std::fs::write(root.join(subject), "Source").unwrap();
            let binding = Binding::build(
                key.clone(),
                subject.into(),
                lk_core::concept::file_revision("Source"),
                &json!({"scope":"same"}),
                &json!({"instructions":"old"}),
                &json!({}),
            )
            .unwrap();
            let session = MutationSession::acquire(root).unwrap();
            let failure =
                Disposition::invocation_failed(super::super::state::InvocationFailure::Exited {
                    engine: lk_core::execution::AgentEngine::Codex,
                    code: 1,
                    failed_at: Some(jiff::Timestamp::now()),
                });
            session
                .apply(binding.plan_record(root, failure).unwrap())
                .unwrap();
            let raw = std::fs::read(root.join(key.path())).unwrap();
            let changed_contract = Binding::build(
                key.clone(),
                subject.into(),
                binding.subject_revision.clone(),
                &json!({"scope":"same"}),
                &json!({"instructions":"new"}),
                &json!({"new":"schema"}),
            )
            .unwrap();
            assert!(changed_contract.held(root).unwrap().is_some());
            assert_eq!(std::fs::read(root.join(key.path())).unwrap(), raw);
            assert!(plan_retry(root, &key, &"b".repeat(64)).is_err());
            std::thread::sleep(std::time::Duration::from_millis(2));
            crate::commands::work::host::record_answer(
                root,
                lk_core::execution::AgentEngine::Codex,
                jiff::Timestamp::now(),
            )
            .unwrap();
            assert!(binding.held(root).unwrap().is_none());
            session
                .apply(
                    binding
                        .plan_record(
                            root,
                            Disposition::invocation_failed(
                                super::super::state::InvocationFailure::Exited {
                                    engine: lk_core::execution::AgentEngine::Codex,
                                    code: 1,
                                    failed_at: Some(jiff::Timestamp::now()),
                                },
                            ),
                        )
                        .unwrap(),
                )
                .unwrap();
            assert!(
                binding
                    .held(root)
                    .unwrap()
                    .unwrap()
                    .contains("again after the host answered")
            );
            let raw = std::fs::read(root.join(key.path())).unwrap();
            let preview = plan_retry(root, &key, &binding.input_revision).unwrap();
            preview.validate_addresses(root).unwrap();
            assert_eq!(std::fs::read(root.join(key.path())).unwrap(), raw);
            std::fs::write(root.join(subject), "Changed source").unwrap();
            assert!(session.apply(preview).is_err());
            assert_eq!(std::fs::read(root.join(key.path())).unwrap(), raw);
            if key.task().is_some() {
                let mut keep = MutationPlan::default();
                assert_eq!(
                    plan_prune_queue(
                        root,
                        &BTreeSet::from([("task".into(), "a".repeat(32))]),
                        &mut keep
                    )
                    .unwrap(),
                    0
                );
                let mut prune = MutationPlan::default();
                assert_eq!(
                    plan_prune_queue(root, &BTreeSet::new(), &mut prune).unwrap(),
                    1
                );
                session.apply(prune).unwrap();
            } else {
                assert_eq!(prune_absent_audits(root, Some(&session)).unwrap(), 0);
                std::fs::remove_file(root.join(subject)).unwrap();
                assert_eq!(prune_absent_audits(root, None).unwrap(), 1);
                assert_eq!(std::fs::read(root.join(key.path())).unwrap(), raw);
                assert_eq!(prune_absent_audits(root, Some(&session)).unwrap(), 1);
            }
            assert!(!root.join(key.path()).exists());
        }
    }

    #[test]
    fn malformed_preparation_state_never_becomes_an_empty_allowance() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let key = Key::AuditDiscovery {
            path: "source.md".into(),
        };
        std::fs::write(root.join("source.md"), "Source").unwrap();
        let binding = Binding::audit_discovery(
            "source.md",
            &lk_core::concept::file_revision("Source"),
            &json!({}),
            &json!({}),
        )
        .unwrap();
        let session = MutationSession::acquire(root).unwrap();
        session
            .apply(
                binding
                    .plan_record(
                        root,
                        Disposition::invocation_failed(
                            super::super::state::InvocationFailure::Exited {
                                engine: lk_core::execution::AgentEngine::Codex,
                                code: 1,
                                failed_at: Some(jiff::Timestamp::now()),
                            },
                        ),
                    )
                    .unwrap(),
            )
            .unwrap();
        let path = root.join(key.path());
        let raw = std::fs::read_to_string(&path).unwrap();
        for invalid in [
            "[]".into(),
            raw.replacen("\"code\":1", "\"code\":2,\"code\":1", 1),
            " ".repeat(MAX_BYTES + 1),
        ] {
            std::fs::write(&path, &invalid).unwrap();
            assert!(binding.held(root).is_err());
            assert!(plan_retry(root, &key, &binding.input_revision).is_err());
            assert!(prune_absent_audits(root, Some(&session)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
    }
}
