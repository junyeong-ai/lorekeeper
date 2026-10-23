use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::commands) enum RejectionKind {
    SelectionInventoryChanged,
    SelectionDecisionsChanged,
    SemanticContextChanged,
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
            && self.ok == (self.rejected == 0)
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
        self.ok &= other.ok;
        self.applied += other.applied;
        self.dropped += other.dropped;
        self.rejected += other.rejected;
        self.quarantined += other.quarantined;
        self.unreadable += other.unreadable;
        self.groups.extend(other.groups);
        if other.fatal.is_some() {
            self.fatal = other.fatal;
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
    let Some(rejection) = error.downcast_ref::<PublicationRejection>() else {
        return Err(miette::miette!("{error}"));
    };
    let identity = TaskIdentity::capture(root, task)?;
    let (receipt, scope) = if let Some(session) = session {
        let (receipt, scope) = super::rejections::record(
            session,
            config,
            config_revision,
            task,
            snapshots,
            rejection,
        )?;
        (Some(receipt), scope)
    } else {
        (
            None,
            super::rejections::observe_scope(root, config, config_revision, task, snapshots)?.0,
        )
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

pub(super) fn retire_group(
    root: &Path,
    task: &lk_queue::QueueTask,
    snapshots: &[ProposalSnapshot],
    session: Option<&lk_vault::MutationSession>,
) -> miette::Result<()> {
    if let Some(session) = session {
        let mut plan = lk_vault::MutationPlan::default();
        super::rejections::remove_matching(root, task, &mut plan)?;
        for snapshot in snapshots {
            plan.remove(&snapshot.proposal.path, snapshot.raw.clone());
        }
        session.apply(plan).into_diagnostic()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
