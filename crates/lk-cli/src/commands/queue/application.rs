use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::commands) enum RejectionKind {
    SelectionInventoryChanged,
    SelectionDecisionsChanged,
    SemanticContextChanged,
    /// A concept's current citations differ from the set its synthesis request was made for.
    CitationsChanged,
    EvidenceScopeChanged,
    TargetChanged,
    ConflictingProposals,
    InvalidProposalIdentity,
    InvalidProposal,
    InvalidResultLink,
    InvalidSection,
}

#[derive(Debug)]
pub(in crate::commands) struct PublicationRejection {
    pub kind: RejectionKind,
    pub detail: String,
}

impl std::fmt::Display for PublicationRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.detail.fmt(formatter)
    }
}

impl std::error::Error for PublicationRejection {}
impl miette::Diagnostic for PublicationRejection {}

pub(in crate::commands) fn rejection(
    kind: RejectionKind,
    detail: impl Into<String>,
) -> miette::Report {
    miette::Report::new(PublicationRejection {
        kind,
        detail: detail.into(),
    })
}

/// Whether a fresh guard refused because semantic inputs changed after they were captured,
/// including a synthesis whose own audit records became unreadable: replanning holds it.
pub(in crate::commands) fn is_semantic_context_changed(error: &miette::Report) -> bool {
    error.downcast_ref::<super::SynthesisHeld>().is_some()
        || matches!(
            error.downcast_ref::<PublicationRejection>(),
            Some(rejection) if matches!(
                rejection.kind,
                RejectionKind::SemanticContextChanged | RejectionKind::CitationsChanged
            )
        )
}

/// Whether a fresh validation found a Work's inputs moved on since its request was prepared:
/// changed semantic context or citations, a changed evidence scope, or a task that is no
/// longer current. A reply prepared from the old inputs is discarded, not a fault.
pub(in crate::commands) fn is_work_input_changed(error: &miette::Report) -> bool {
    is_semantic_context_changed(error)
        || error.downcast_ref::<super::TaskNotCurrent>().is_some()
        || matches!(
            error.downcast_ref::<PublicationRejection>(),
            Some(rejection) if rejection.kind == RejectionKind::EvidenceScopeChanged
        )
}

/// Whether a synthesis request was refused because its concept's citations have since changed.
pub(in crate::commands) fn is_citations_changed(error: &miette::Report) -> bool {
    matches!(
        error.downcast_ref::<PublicationRejection>(),
        Some(rejection) if rejection.kind == RejectionKind::CitationsChanged
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct TaskIdentity {
    pub task_id: String,
    pub cache_hash: String,
    pub target: lk_queue::TaskTarget,
    pub owner: String,
}

impl TaskIdentity {
    pub(super) fn is_valid(&self) -> bool {
        let owner = Path::new(&self.owner);
        !self.task_id.trim().is_empty()
            && self.cache_hash.len() == 32
            && self
                .cache_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && super::resolve_target_path(&self.target.vault_path).is_some()
            && owner.extension().is_some_and(|extension| extension == "md")
            && owner
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
    }

    pub(super) fn capture(root: &Path, task: &lk_queue::QueueTask) -> miette::Result<Self> {
        task.validate_retained_input()
            .map_err(|error| miette::miette!("{error}"))?;
        task.generation_policy()
            .map_err(|error| miette::miette!("{error}"))?;
        let root = root.canonicalize().into_diagnostic()?;
        let path = super::resolve_target_path(&task.target.vault_path)
            .ok_or_else(|| miette::miette!("invalid retained task target"))?;
        let full = lk_vault::writable_page_path(&root, &path).into_diagnostic()?;
        let owner = full.canonicalize().into_diagnostic()?;
        let owner = owner.strip_prefix(root).into_diagnostic()?;
        Ok(Self {
            task_id: task.task_id.clone(),
            cache_hash: task.cache_hash.clone(),
            target: task.target.clone(),
            owner: owner
                .to_str()
                .ok_or_else(|| miette::miette!("task owner is not UTF-8"))?
                .into(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct Proposal {
    pub path: String,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct RejectionScope {
    pub revision: String,
    pub owners: Vec<String>,
}

impl RejectionScope {
    pub(super) fn is_valid(&self) -> bool {
        self.revision.len() == 64
            && self
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && !self.owners.is_empty()
            && self.owners.windows(2).all(|pair| pair[0] < pair[1])
            && self.owners.iter().all(|owner| {
                !owner.is_empty()
                    && Path::new(owner)
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
            })
    }
}

pub(super) struct ProposalSnapshot {
    pub proposal: Proposal,
    pub raw: String,
}

pub(super) fn proposal_snapshot(root: &Path, path: &Path) -> miette::Result<ProposalSnapshot> {
    let root = root.canonicalize().into_diagnostic()?;
    let full = std::path::absolute(path).into_diagnostic()?;
    let metadata = std::fs::symlink_metadata(&full).into_diagnostic()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(miette::miette!(
            "staged proposals must be direct regular files"
        ));
    }
    let canonical = full.canonicalize().into_diagnostic()?;
    let relative = canonical.strip_prefix(&root).into_diagnostic()?;
    lk_vault::page_path(&root, relative).into_diagnostic()?;
    let bytes = std::fs::read(full).into_diagnostic()?;
    let revision = blake3::hash(&bytes).to_hex().to_string();
    let raw = String::from_utf8(bytes).into_diagnostic()?;
    Ok(ProposalSnapshot {
        proposal: Proposal {
            path: relative
                .to_str()
                .ok_or_else(|| miette::miette!("proposal path is not UTF-8"))?
                .into(),
            revision,
        },
        raw,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands) enum Outcome {
    Applied,
    Dropped {
        reason: String,
    },
    /// The exact request this result answers is no longer pending: it was retired while the
    /// result was being drafted. The result is quarantined, not published or judged.
    Orphaned {
        reason: String,
    },
    /// The result answers a read scope or target its still-pending request no longer has. The
    /// result is kept under `results/superseded/` and the request is drafted again.
    Superseded {
        kind: RejectionKind,
        detail: String,
    },
    /// The target's own inputs cannot be read, so the result can be neither judged nor
    /// published. It stays staged and applies once they are repaired.
    Held {
        detail: String,
    },
    Rejected {
        kind: RejectionKind,
        detail: String,
        scope: RejectionScope,
        receipt: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(in crate::commands) struct Group {
    pub identity: Option<TaskIdentity>,
    pub proposals: Vec<Proposal>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl<'de> Deserialize<'de> for Group {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "outcome", rename_all = "kebab-case", deny_unknown_fields)]
        enum Wire {
            Applied {
                identity: Option<TaskIdentity>,
                proposals: Vec<Proposal>,
            },
            Dropped {
                identity: Option<TaskIdentity>,
                proposals: Vec<Proposal>,
                reason: String,
            },
            Orphaned {
                identity: Option<TaskIdentity>,
                proposals: Vec<Proposal>,
                reason: String,
            },
            Superseded {
                identity: Option<TaskIdentity>,
                proposals: Vec<Proposal>,
                kind: RejectionKind,
                detail: String,
            },
            Held {
                identity: Option<TaskIdentity>,
                proposals: Vec<Proposal>,
                detail: String,
            },
            Rejected {
                identity: Option<TaskIdentity>,
                proposals: Vec<Proposal>,
                kind: RejectionKind,
                detail: String,
                scope: RejectionScope,
                receipt: Option<String>,
            },
        }
        let (identity, proposals, outcome) = match Wire::deserialize(deserializer)? {
            Wire::Applied {
                identity,
                proposals,
            } => (identity, proposals, Outcome::Applied),
            Wire::Dropped {
                identity,
                proposals,
                reason,
            } => (identity, proposals, Outcome::Dropped { reason }),
            Wire::Orphaned {
                identity,
                proposals,
                reason,
            } => (identity, proposals, Outcome::Orphaned { reason }),
            Wire::Superseded {
                identity,
                proposals,
                kind,
                detail,
            } => (identity, proposals, Outcome::Superseded { kind, detail }),
            Wire::Held {
                identity,
                proposals,
                detail,
            } => (identity, proposals, Outcome::Held { detail }),
            Wire::Rejected {
                identity,
                proposals,
                kind,
                detail,
                scope,
                receipt,
            } => (
                identity,
                proposals,
                Outcome::Rejected {
                    kind,
                    detail,
                    scope,
                    receipt,
                },
            ),
        };
        Ok(Self {
            identity,
            proposals,
            outcome,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct Fatal {
    pub detail: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct Report {
    pub ok: bool,
    pub dry_run: bool,
    pub applied: usize,
    pub dropped: usize,
    pub rejected: usize,
    /// Groups quarantined because their request was retired; independent work continues.
    pub orphaned: usize,
    /// Groups set aside because their pending request's read scope changed; it is redrafted
    /// and independent work continues.
    pub superseded: usize,
    /// Groups left staged because their target's own inputs cannot be read.
    pub held: usize,
    pub quarantined: usize,
    pub unreadable: usize,
    pub groups: Vec<Group>,
    pub fatal: Option<Fatal>,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            ok: true,
            dry_run: false,
            applied: 0,
            dropped: 0,
            rejected: 0,
            orphaned: 0,
            superseded: 0,
            held: 0,
            quarantined: 0,
            unreadable: 0,
            groups: Vec::new(),
            fatal: None,
        }
    }
}

impl Report {
    pub(super) fn new(dry_run: bool) -> Self {
        Self {
            ok: true,
            dry_run,
            ..Default::default()
        }
    }

    pub(in crate::commands) fn can_continue(&self) -> bool {
        !self.dry_run
            && self.fatal.is_none()
            && self.unreadable == 0
            && self.quarantined == 0
            && self.applied
                == self
                    .groups
                    .iter()
                    .filter(|group| matches!(group.outcome, Outcome::Applied))
                    .count()
            && self.dropped
                == self
                    .groups
                    .iter()
                    .filter(|group| matches!(group.outcome, Outcome::Dropped { .. }))
                    .count()
            && self.rejected
                == self
                    .groups
                    .iter()
                    .filter(|group| matches!(group.outcome, Outcome::Rejected { .. }))
                    .count()
            && self.orphaned
                == self
                    .groups
                    .iter()
                    .filter(|group| matches!(group.outcome, Outcome::Orphaned { .. }))
                    .count()
            && self.superseded
                == self
                    .groups
                    .iter()
                    .filter(|group| matches!(group.outcome, Outcome::Superseded { .. }))
                    .count()
            && self.held
                == self
                    .groups
                    .iter()
                    .filter(|group| matches!(group.outcome, Outcome::Held { .. }))
                    .count()
            && self.ok
                == (self.rejected == 0
                    && self.orphaned == 0
                    && self.superseded == 0
                    && self.held == 0)
            && self.groups.iter().all(|group| match &group.outcome {
                Outcome::Rejected { receipt, scope, .. } => {
                    group.identity.as_ref().is_some_and(TaskIdentity::is_valid)
                        && scope.is_valid()
                        && !group.proposals.is_empty()
                        && group.proposals.iter().all(|proposal| {
                            Path::new(&proposal.path).parent()
                                == Some(Path::new(".lorekeeper/queue/results"))
                                && proposal.revision.len() == 64
                                && proposal.revision.bytes().all(|byte| {
                                    byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
                                })
                        })
                        && receipt.as_ref().is_some_and(|path| {
                            Path::new(path).parent()
                                == Some(Path::new(".lorekeeper/queue/rejections"))
                        })
                }
                _ => true,
            })
    }

    pub(super) fn fail(&mut self, error: impl std::fmt::Display) {
        self.ok = false;
        self.fatal = Some(Fatal {
            detail: error.to_string(),
        });
    }

    pub(super) fn append(&mut self, other: Self) {
        let Self {
            ok,
            dry_run: _,
            applied,
            dropped,
            rejected,
            orphaned,
            superseded,
            held,
            quarantined,
            unreadable,
            groups,
            fatal,
        } = other;
        self.ok &= ok;
        self.applied += applied;
        self.dropped += dropped;
        self.rejected += rejected;
        self.orphaned += orphaned;
        self.superseded += superseded;
        self.held += held;
        self.quarantined += quarantined;
        self.unreadable += unreadable;
        self.groups.extend(groups);
        if fatal.is_some() {
            self.fatal = fatal;
        }
    }

    pub(super) fn outcome(
        &mut self,
        identity: Option<TaskIdentity>,
        snapshots: &[ProposalSnapshot],
        outcome: Outcome,
    ) {
        match &outcome {
            Outcome::Applied => self.applied += 1,
            Outcome::Dropped { .. } => self.dropped += 1,
            Outcome::Orphaned { .. } => {
                self.orphaned += 1;
                self.ok = false;
            }
            Outcome::Superseded { .. } => {
                self.superseded += 1;
                self.ok = false;
            }
            Outcome::Held { .. } => {
                self.held += 1;
                self.ok = false;
            }
            Outcome::Rejected { .. } => {
                self.rejected += 1;
                self.ok = false;
            }
        }
        self.groups.push(Group {
            identity,
            proposals: snapshots
                .iter()
                .map(|snapshot| snapshot.proposal.clone())
                .collect(),
            outcome,
        });
    }
}

pub(super) fn capture_group<T: Serialize + serde::de::DeserializeOwned>(
    root: &Path,
    values: &[(PathBuf, T)],
) -> miette::Result<Vec<ProposalSnapshot>> {
    let mut snapshots = Vec::new();
    for (path, expected) in values {
        let snapshot = proposal_snapshot(root, path)?;
        let actual: T = serde_json::from_str(&snapshot.raw).into_diagnostic()?;
        let actual = serde_json::to_value(actual).into_diagnostic()?;
        let expected = serde_json::to_value(expected).into_diagnostic()?;
        if actual != expected {
            return Err(miette::miette!(
                "staged result changed during application: {}",
                path.display()
            ));
        }
        snapshots.push(snapshot);
    }
    snapshots.sort_by(|left, right| left.proposal.cmp(&right.proposal));
    Ok(snapshots)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn reject_group(
    report: &mut Report,
    root: &Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    task: &lk_queue::QueueTask,
    snapshots: &[ProposalSnapshot],
    error: &miette::Report,
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<()> {
    if let Some(held) = error.downcast_ref::<super::SynthesisHeld>() {
        return hold_group(report, root, task, snapshots, held);
    }
    let Some(rejection) = error.downcast_ref::<PublicationRejection>() else {
        return Err(miette::miette!("{error}"));
    };
    let identity = TaskIdentity::capture(root, task)?;
    if task.kind == lk_queue::TaskKind::SynthesizeConcept
        && matches!(
            rejection.kind,
            RejectionKind::SemanticContextChanged | RejectionKind::TargetChanged
        )
    {
        let mut plan = lk_vault::MutationPlan::default();
        super::rejections::remove_matching(root, task, &mut plan)?;
        quarantine_group(
            root,
            ResultStore::Superseded,
            snapshots,
            &format!("{}\n{SUPERSEDED}", rejection.detail),
            plan,
            session,
        )?;
        report.outcome(
            Some(identity),
            snapshots,
            Outcome::Superseded {
                kind: rejection.kind,
                detail: rejection.detail.clone(),
            },
        );
        return Ok(());
    }
    let recorded = match session {
        Some(session) => {
            super::rejections::record(session, config, config_revision, task, snapshots, rejection)
                .map(|(receipt, scope)| (Some(receipt), scope))
        }
        None => super::rejections::observe_scope(root, config, config_revision, task, snapshots)
            .map(|(scope, _)| (None, scope)),
    };
    let (receipt, scope) = match recorded {
        Ok(recorded) => recorded,
        Err(error) => match error.downcast_ref::<super::SynthesisHeld>() {
            Some(held) => return hold_group(report, root, task, snapshots, held),
            None => return Err(error),
        },
    };
    report.outcome(
        Some(identity),
        snapshots,
        Outcome::Rejected {
            kind: rejection.kind,
            detail: rejection.detail.clone(),
            scope,
            receipt,
        },
    );
    Ok(())
}

/// A group whose target's own inputs cannot be read stays staged, judged once they are repaired.
fn hold_group(
    report: &mut Report,
    root: &Path,
    task: &lk_queue::QueueTask,
    snapshots: &[ProposalSnapshot],
    held: &super::SynthesisHeld,
) -> miette::Result<()> {
    report.outcome(
        Some(TaskIdentity::capture(root, task)?),
        snapshots,
        Outcome::Held {
            detail: held.to_string(),
        },
    );
    Ok(())
}

/// Why a synthesis result whose request's read scope changed is set aside.
const SUPERSEDED: &str = "the result answers a read scope or target its pending request no longer has; it is kept under results/superseded and the request is drafted again from its current scope";

/// A results subdirectory that keeps what apply will not publish, beside each reason.
#[derive(Clone, Copy)]
pub(super) enum ResultStore {
    /// The result's exact request is no longer pending.
    Orphaned,
    /// The result answers a read scope or target its pending request no longer has.
    Superseded,
}

impl ResultStore {
    fn directory(self) -> &'static str {
        match self {
            Self::Orphaned => "orphaned",
            Self::Superseded => "superseded",
        }
    }
}

/// Move a group's results into `store` beside `reason`, together with `plan`'s steps, as one
/// guarded plan. Each move expects the bytes the group was judged from, and a preview
/// validates the same plan an apply journals, so neither follows a linked or replaced store.
pub(super) fn quarantine_group(
    root: &Path,
    store: ResultStore,
    snapshots: &[ProposalSnapshot],
    reason: &str,
    mut plan: lk_vault::MutationPlan,
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<()> {
    let relative = Path::new(".lorekeeper/queue")
        .join(lk_queue::RESULTS_SUBDIR)
        .join(store.directory());
    super::quarantine_store(root, &relative)?;
    let directory = root.join(&relative);
    let mut taken = std::collections::BTreeSet::new();
    let mut moves = Vec::new();
    for snapshot in snapshots {
        let source = Path::new(&snapshot.proposal.path);
        let (destination, reason_path) = super::quarantine_names(&directory, source, &mut taken);
        let name = |path: &Path| relative.join(path.file_name().unwrap_or_default());
        plan.rename(source, name(&destination), snapshot.raw.clone());
        plan.create(name(&reason_path), format!("{reason}\n"));
        moves.push((source.to_path_buf(), destination));
    }
    match session {
        None => plan.validate_addresses(root),
        Some(session) => session.apply(plan),
    }
    .map_err(|error| {
        miette::miette!(
            "cannot keep results under results/{}: {error}",
            store.directory()
        )
    })?;
    if session.is_some() {
        for (source, destination) in moves {
            eprintln!("  kept {} → {}", source.display(), destination.display());
        }
    }
    Ok(())
}

pub(super) fn retire_group(
    root: &Path,
    task: &lk_queue::QueueTask,
    snapshots: &[ProposalSnapshot],
    classification: super::classification::CapturedClassification,
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<()> {
    let plan = plan_retirement(root, task, snapshots, classification)?;
    if let Some(session) = session {
        session.apply(plan).into_diagnostic()?;
    } else {
        plan.validate_addresses(root).into_diagnostic()?;
    }
    Ok(())
}

fn plan_retirement(
    root: &Path,
    task: &lk_queue::QueueTask,
    snapshots: &[ProposalSnapshot],
    classification: super::classification::CapturedClassification,
) -> miette::Result<lk_vault::MutationPlan> {
    let mut plan = classification.retirement_plan()?;
    super::rejections::remove_matching(root, task, &mut plan)?;
    for snapshot in snapshots {
        plan.remove(&snapshot.proposal.path, snapshot.raw.clone());
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct RetirementFixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        task: lk_queue::QueueTask,
        target: PathBuf,
        current: String,
        snapshots: Vec<ProposalSnapshot>,
        receipt: PathBuf,
        receipt_raw: String,
    }

    impl RetirementFixture {
        fn new(synthesis: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let queue = lk_queue::QueueLlmClient::new(root.join(".lorekeeper/queue"));
            let task = if synthesis {
                let raw = "---\ntype: document\n---\nOriginal evidence.\n";
                std::fs::write(root.join("evidence.md"), raw).unwrap();
                queue.build_task(
                    "retirement",
                    lk_queue::TaskKind::SynthesizeConcept,
                    lk_queue::ConceptSynthesisRequest {
                        citations: vec![lk_core::concept::Citation::new(
                            Path::new("evidence.md"),
                            &lk_vault::parse_page(raw).unwrap(),
                        )],
                        locale: lk_core::i18n::Locale::En,
                        related_anchor: Some("## Related Concepts".into()),
                        target: lk_queue::TaskTarget {
                            vault_path: "target.md".into(),
                            kind: lk_queue::TargetKind::ConceptSynthesis,
                            anchor: "## Synthesis".into(),
                        },
                    },
                )
            } else {
                queue.build_task(
                    "retirement",
                    lk_queue::TaskKind::Summarize,
                    lk_queue::SummarizeRequest {
                        text: "Retained source.".into(),
                        max_sentences: 3,
                        focus: None,
                        locale: lk_core::i18n::Locale::En,
                        source_type: None,
                        target: lk_queue::TaskTarget {
                            vault_path: "target.md".into(),
                            kind: lk_queue::TargetKind::DocumentSummary,
                            anchor: "## Summary".into(),
                        },
                    },
                )
            };
            let current = format!(
                "---\ntype: {}\nllm_inputs:\n  {}: {}\n---\n{}\n\n## Related Concepts\n",
                task.target.kind.page_format(),
                task.target.kind.llm_inputs_key(),
                task.cache_hash,
                task.target.anchor
            );
            let target = PathBuf::from(&task.target.vault_path);
            std::fs::write(root.join(&target), &current).unwrap();
            let relative = PathBuf::from(if synthesis {
                ".lorekeeper/queue/results/answer.synthesis.json"
            } else {
                ".lorekeeper/queue/results/answer.section.json"
            });
            std::fs::create_dir_all(root.join(relative.parent().unwrap())).unwrap();
            let result = if synthesis {
                serde_json::to_string(&lk_queue::SynthesisResult {
                    task_id: task.task_id.clone(),
                    cache_hash: task.cache_hash.clone(),
                    target_revision: lk_core::concept::file_revision(&current),
                    context_revision: "a".repeat(64),
                    synthesis: "Authored answer.\n".into(),
                    related: Some("Related evidence.\n".into()),
                })
                .unwrap()
            } else {
                serde_json::to_string(&lk_queue::SectionResult {
                    task_id: task.task_id.clone(),
                    cache_hash: task.cache_hash.clone(),
                    target_revision: lk_core::concept::file_revision(&current),
                    context_revision: Some("a".repeat(64)),
                    body: "Authored answer.\n".into(),
                })
                .unwrap()
            };
            std::fs::write(root.join(&relative), result).unwrap();
            let snapshots = vec![proposal_snapshot(&root, &root.join(&relative)).unwrap()];
            let digest =
                blake3::hash(&serde_json::to_vec(&(&task.task_id, &task.cache_hash)).unwrap())
                    .to_hex()
                    .to_string();
            let receipt = PathBuf::from(format!(".lorekeeper/queue/rejections/{digest}.json"));
            std::fs::create_dir_all(root.join(receipt.parent().unwrap())).unwrap();
            let receipt_raw = json!({"schema":"queue-application-rejection-v1","validation_policy":"1".repeat(64),"config_revision":"2".repeat(64),"task_revision":"3".repeat(64),"identity":TaskIdentity::capture(&root,&task).unwrap(),"proposals":[snapshots[0].proposal],"scope":{"revision":"4".repeat(64),"owners":[task.target.vault_path]},"kind":"target-changed","reason":"Retained refusal."}).to_string();
            std::fs::write(root.join(&receipt), &receipt_raw).unwrap();
            std::fs::create_dir_all(root.join(".lorekeeper/work/paid")).unwrap();
            std::fs::write(
                root.join(".lorekeeper/work/paid/node.json"),
                "original paid bytes\n",
            )
            .unwrap();
            Self {
                _directory: directory,
                root,
                task,
                target,
                current,
                snapshots,
                receipt,
                receipt_raw,
            }
        }

        fn plan(&self) -> lk_vault::MutationPlan {
            let classification = super::super::classification::capture(
                &self.root,
                &self.task,
                &mut Default::default(),
            )
            .unwrap();
            let status = classification.status();
            assert!(matches!(
                status,
                super::super::TaskStatus::Done
                    | super::super::TaskStatus::Stale
                    | super::super::TaskStatus::MissingTarget
            ));
            plan_retirement(&self.root, &self.task, &self.snapshots, classification).unwrap()
        }

        fn preserve(&self) {
            for snapshot in &self.snapshots {
                assert_eq!(
                    std::fs::read_to_string(self.root.join(&snapshot.proposal.path)).unwrap(),
                    snapshot.raw
                );
            }
            assert_eq!(
                std::fs::read_to_string(self.root.join(&self.receipt)).unwrap(),
                self.receipt_raw
            );
            assert_eq!(
                std::fs::read_to_string(self.root.join(".lorekeeper/work/paid/node.json")).unwrap(),
                "original paid bytes\n"
            );
        }

        fn refuse_after_restore(&self, plan: lk_vault::MutationPlan, recover: bool) {
            if recover {
                let journal = json!({"version":1,"plan":plan}).to_string();
                let path = self.root.join(".lorekeeper/mutation/pending.json");
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &journal).unwrap();
                assert!(lk_vault::MutationSession::acquire(&self.root).is_err());
                assert_eq!(std::fs::read_to_string(path).unwrap(), journal);
            } else {
                let session = lk_vault::MutationSession::acquire(&self.root).unwrap();
                assert!(session.apply(plan).is_err());
                assert!(!self.root.join(".lorekeeper/mutation/pending.json").exists());
            }
            self.preserve();
        }
    }

    #[test]
    fn result_retirement_guards_target_decisions_before_apply_and_recovery() {
        for decision in ["done", "stale", "policy", "missing-section", "missing-file"] {
            for recover in [false, true] {
                let fixture = RetirementFixture::new(false);
                let changed = match decision {
                    "done" => fixture.current.replace("---\n##",&format!("  summary_done: {}\n---\n##",fixture.task.cache_hash)),
                    "stale" => fixture.current.replace(&fixture.task.cache_hash,"other"),
                    "policy" => fixture.current.replace("---\n##",&format!("  summary_policy_requested_input: {}\n  summary_policy_requested: future\n---\n##",fixture.task.cache_hash)),
                    "missing-section" => fixture.current.replace("## Summary","## Other"),
                    "missing-file" => String::new(),
                    _ => unreachable!(),
                };
                if decision == "missing-file" {
                    std::fs::remove_file(fixture.root.join(&fixture.target)).unwrap();
                } else {
                    std::fs::write(fixture.root.join(&fixture.target), changed).unwrap();
                }
                let plan = fixture.plan();
                std::fs::write(fixture.root.join(&fixture.target), &fixture.current).unwrap();
                fixture.refuse_after_restore(plan, recover);
            }
        }
    }

    #[test]
    fn result_retirement_guards_stale_sources_before_apply_and_recovery() {
        for recover in [false, true] {
            let fixture = RetirementFixture::new(true);
            let source = fixture.root.join("evidence.md");
            let original = std::fs::read_to_string(&source).unwrap();
            std::fs::write(&source, original.replace("Original", "Changed")).unwrap();
            let plan = fixture.plan();
            std::fs::write(source, original).unwrap();
            fixture.refuse_after_restore(plan, recover);
        }
    }

    #[test]
    fn guarded_result_retirement_recovers_every_unchanged_prefix() {
        let seed = RetirementFixture::new(false);
        std::fs::remove_file(seed.root.join(&seed.target)).unwrap();
        let count = serde_json::to_value(seed.plan()).unwrap()["steps"]
            .as_array()
            .unwrap()
            .len();
        for prefix in 0..=count {
            let fixture = RetirementFixture::new(false);
            std::fs::remove_file(fixture.root.join(&fixture.target)).unwrap();
            let plan = fixture.plan();
            let value = serde_json::to_value(plan).unwrap();
            let state = fixture.root.join(".lorekeeper/mutation");
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(
                state.join("pending.json"),
                json!({"version":1,"plan":value}).to_string(),
            )
            .unwrap();
            for step in &value["steps"].as_array().unwrap()[..prefix] {
                if step["kind"] == "remove" {
                    std::fs::remove_file(fixture.root.join(step["path"].as_str().unwrap()))
                        .unwrap();
                }
            }
            let _session = lk_vault::MutationSession::acquire(&fixture.root).unwrap();
            assert!(!fixture.root.join(&fixture.receipt).exists());
            assert!(
                fixture
                    .snapshots
                    .iter()
                    .all(|snapshot| !fixture.root.join(&snapshot.proposal.path).exists())
            );
            assert!(!state.join("pending.json").exists());
            assert_eq!(
                std::fs::read_to_string(fixture.root.join(".lorekeeper/work/paid/node.json"))
                    .unwrap(),
                "original paid bytes\n"
            );
        }
    }

    #[test]
    fn result_retirement_refusal_preserves_prior_independent_publication() {
        let fixture = RetirementFixture::new(false);
        std::fs::write(fixture.root.join("independent.md"), "before").unwrap();
        let session = lk_vault::MutationSession::acquire(&fixture.root).unwrap();
        let mut publication = lk_vault::MutationPlan::default();
        publication.edit(
            "independent.md",
            "before".into(),
            "published answer and completion\n".into(),
        );
        session.apply(publication).unwrap();
        std::fs::remove_file(fixture.root.join(&fixture.target)).unwrap();
        let plan = fixture.plan();
        std::fs::write(fixture.root.join(&fixture.target), &fixture.current).unwrap();
        assert!(session.apply(plan).is_err());
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("independent.md")).unwrap(),
            "published answer and completion\n"
        );
        fixture.preserve();
    }

    #[test]
    fn result_admission_and_completed_task_retirement_share_one_capture() {
        let fixture = RetirementFixture::new(false);
        let completed = fixture.current.replace(
            "---\n##",
            &format!("  summary_done: {}\n---\n##", fixture.task.cache_hash),
        );
        std::fs::write(fixture.root.join(&fixture.target), completed).unwrap();
        let captured =
            super::super::classification::capture_result(&fixture.root, &fixture.task).unwrap();
        assert_eq!(captured.status(), super::super::TaskStatus::Current);
        assert_eq!(captured.task_status(), super::super::TaskStatus::Done);
        std::fs::write(fixture.root.join(&fixture.target), &fixture.current).unwrap();
        let plan = plan_retirement(
            &fixture.root,
            &fixture.task,
            &fixture.snapshots,
            captured.into_completed_task().unwrap(),
        )
        .unwrap();
        fixture.refuse_after_restore(plan, false);
    }

    #[test]
    fn result_retirement_refuses_unknown_receipts_before_deletion() {
        for raw in ["{broken", "{\"schema\":\"future\"}"] {
            let mut fixture = RetirementFixture::new(false);
            std::fs::remove_file(fixture.root.join(&fixture.target)).unwrap();
            fixture.receipt_raw = raw.into();
            std::fs::write(fixture.root.join(&fixture.receipt), raw).unwrap();
            let captured = super::super::classification::capture(
                &fixture.root,
                &fixture.task,
                &mut Default::default(),
            )
            .unwrap();
            assert!(
                plan_retirement(&fixture.root, &fixture.task, &fixture.snapshots, captured)
                    .is_err()
            );
            fixture.preserve();
            assert!(
                !fixture
                    .root
                    .join(".lorekeeper/mutation/pending.json")
                    .exists()
            );
        }
    }

    fn identity() -> TaskIdentity {
        TaskIdentity {
            task_id: "task".into(),
            cache_hash: "a".repeat(32),
            target: lk_queue::TaskTarget {
                vault_path: "wiki/documents/source.md".into(),
                kind: lk_queue::TargetKind::DocumentSummary,
                anchor: "## Summary".into(),
            },
            owner: "wiki/documents/source.md".into(),
        }
    }

    fn snapshots() -> Vec<ProposalSnapshot> {
        vec![ProposalSnapshot {
            proposal: Proposal {
                path: ".lorekeeper/queue/results/task.section.json".into(),
                revision: "b".repeat(64),
            },
            raw: "{}".into(),
        }]
    }

    fn rejected() -> Report {
        let mut report = Report::new(false);
        report.outcome(
            Some(identity()),
            &snapshots(),
            Outcome::Rejected {
                kind: RejectionKind::TargetChanged,
                detail: "changed".into(),
                scope: RejectionScope {
                    revision: "c".repeat(64),
                    owners: vec!["wiki/documents/source.md".into()],
                },
                receipt: Some(".lorekeeper/queue/rejections/receipt.json".into()),
            },
        );
        report
    }

    #[test]
    fn application_report_roundtrips_every_outcome_and_rejects_unknown_fields() {
        let mut report = rejected();
        report.outcome(Some(identity()), &snapshots(), Outcome::Applied);
        report.outcome(
            None,
            &snapshots(),
            Outcome::Dropped {
                reason: "stale".into(),
            },
        );
        let value = serde_json::to_value(&report).unwrap();
        let decoded: Report = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        for index in 0..3 {
            let mut invalid = value.clone();
            invalid["groups"][index]["unknown"] = true.into();
            assert!(serde_json::from_value::<Report>(invalid).is_err());
        }
        let mut invalid = value;
        invalid["unknown"] = true.into();
        assert!(serde_json::from_value::<Report>(invalid).is_err());
    }

    #[test]
    fn application_report_continues_only_with_complete_trusted_rejection_metadata() {
        assert!(Report::new(false).can_continue());
        assert!(rejected().can_continue());
        let mutations: [fn(&mut Report); 9] = [
            |report| report.groups[0].identity = None,
            |report| report.groups[0].identity.as_mut().unwrap().owner = "../outside.md".into(),
            |report| report.groups[0].identity.as_mut().unwrap().task_id.clear(),
            |report| report.groups[0].proposals.clear(),
            |report| report.groups[0].proposals[0].revision.clear(),
            |report| {
                if let Outcome::Rejected { receipt, .. } = &mut report.groups[0].outcome {
                    *receipt = None
                }
            },
            |report| report.rejected += 1,
            |report| report.dry_run = true,
            |report| report.fail("IO failure"),
        ];
        for mutate in mutations {
            let mut report = rejected();
            mutate(&mut report);
            assert!(!report.can_continue());
        }
    }

    #[test]
    fn group_capture_preserves_exact_bytes_while_comparing_typed_optional_values() {
        #[derive(Serialize, Deserialize)]
        struct Value {
            value: Option<String>,
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proposal.json");
        std::fs::write(&path, "{\"value\":null}\n").unwrap();
        let captured = capture_group(directory.path(), &[(path, Value { value: None })]).unwrap();
        assert_eq!(captured[0].raw, "{\"value\":null}\n");
        assert_eq!(
            captured[0].proposal.revision,
            blake3::hash(captured[0].raw.as_bytes())
                .to_hex()
                .to_string()
        );
    }
}
