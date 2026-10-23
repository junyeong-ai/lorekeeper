use std::path::{Path, PathBuf};

use lk_graph::audit::{AuditCandidate, AuditPlan, AuditSubjectKind};
use serde::Serialize;

use super::queue::readiness::InputReadiness;

const MAX_DEFERRED_DETAILS: usize = 100;

#[derive(Clone, Serialize)]
pub(super) struct DeferredAudit {
    pub kind: AuditSubjectKind,
    pub path: String,
    pub file_revision: String,
    pub input_revision: String,
    pub reasons: Vec<String>,
}

pub(super) struct InputAvailability {
    root: PathBuf,
    readiness: InputReadiness,
    deferred: Vec<DeferredAudit>,
    deferred_count: usize,
}

impl InputAvailability {
    pub fn load(root: &Path) -> miette::Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            readiness: InputReadiness::load(root)?,
            deferred: Vec::new(),
            deferred_count: 0,
        })
    }

    pub fn check(&mut self, candidate: &AuditCandidate) -> Result<bool, lk_graph::GraphError> {
        let reasons = self
            .readiness
            .unfinished(&self.root, candidate)
            .map_err(|error| lk_graph::GraphError::Io(error.to_string()))?;
        if reasons.is_empty() {
            return Ok(true);
        }
        self.deferred_count += 1;
        if self.deferred.len() < MAX_DEFERRED_DETAILS {
            self.deferred.push(DeferredAudit {
                kind: candidate.kind,
                path: candidate.path.clone(),
                file_revision: candidate.file_revision.clone(),
                input_revision: candidate.input_revision.clone(),
                reasons,
            });
        }
        Ok(false)
    }

    pub fn require_ready(&mut self, candidate: &AuditCandidate) -> miette::Result<()> {
        self.readiness.require_ready(&self.root, candidate)
    }

    pub fn guard(&self, mutation: &mut lk_vault::MutationPlan) -> miette::Result<()> {
        self.readiness.guard(&self.root, mutation)
    }

    pub fn deferred(&self) -> &[DeferredAudit] {
        &self.deferred
    }

    pub fn deferred_count(&self) -> usize {
        self.deferred_count
    }
}

#[derive(Serialize)]
pub(super) struct ReadyAuditPlan {
    #[serde(flatten)]
    plan: AuditPlan,
    readiness: AuditReadiness,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
enum ReadinessScope {
    Encountered,
    Selection,
}

#[derive(Serialize)]
struct AuditReadiness {
    scope: ReadinessScope,
    ready: Vec<ReadyAudit>,
    deferred: Vec<DeferredAudit>,
    deferred_count: usize,
}

#[derive(Serialize)]
struct ReadyAudit {
    kind: AuditSubjectKind,
    path: String,
    file_revision: String,
    input_revision: String,
}

impl From<&AuditCandidate> for ReadyAudit {
    fn from(candidate: &AuditCandidate) -> Self {
        Self {
            kind: candidate.kind,
            path: candidate.path.clone(),
            file_revision: candidate.file_revision.clone(),
            input_revision: candidate.input_revision.clone(),
        }
    }
}

pub(super) fn plan_ready(
    root: &Path,
    config: &lk_core::config::Config,
    pages: &[lk_graph::scan::ScannedPage],
    limit: usize,
    paths: &[String],
) -> miette::Result<ReadyAuditPlan> {
    let mut availability = InputAvailability::load(root)?;
    let mut ready = Vec::new();
    let plan = if paths.is_empty() {
        let plan = lk_graph::audit::plan_available_partitioned(
            root,
            &config.vault.dirs,
            pages,
            limit,
            config.graph.audit.max_input_bytes,
            &mut |candidate| availability.check(candidate),
        )
        .map_err(|error| miette::miette!("{error}"))?;
        ready.extend(plan.candidates.iter().map(ReadyAudit::from));
        plan
    } else {
        let plan = lk_graph::audit::plan_selected_partitioned(
            root,
            &config.vault.dirs,
            pages,
            limit,
            paths,
            config.graph.audit.max_input_bytes,
        )
        .map_err(|error| miette::miette!("{error}"))?;
        for candidate in &plan.candidates {
            if availability
                .check(candidate)
                .map_err(|error| miette::miette!("{error}"))?
            {
                ready.push(ReadyAudit::from(candidate));
            }
        }
        plan
    };
    Ok(ReadyAuditPlan {
        plan,
        readiness: AuditReadiness {
            scope: if paths.is_empty() {
                ReadinessScope::Encountered
            } else {
                ReadinessScope::Selection
            },
            ready,
            deferred: availability.deferred().to_vec(),
            deferred_count: availability.deferred_count(),
        },
    })
}
