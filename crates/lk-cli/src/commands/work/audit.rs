use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{Prepared, Subject, Work};
use crate::commands::execution::audit::{self, Judgment};

pub(super) fn validate_result_capacity(
    candidate: &lk_graph::audit::AuditCandidate,
    limit: usize,
) -> miette::Result<()> {
    let assessment = lk_graph::audit::AuditAssessment {
        path: candidate.path.clone(),
        file_revision: candidate.file_revision.clone(),
        input_revision: candidate.input_revision.clone(),
        disposition: lk_graph::audit::AuditDisposition::Reviewed,
        reason: String::new(),
        reviews: candidate
            .reviews
            .iter()
            .map(|review| review.path.clone())
            .collect(),
    };
    audit::response_limit(&[assessment], limit).map(|_| ())
}

fn assessment(prepared: &Prepared) -> miette::Result<lk_graph::audit::AuditAssessment> {
    let Subject::Audit {
        path,
        file_revision,
        input_revision,
    } = &prepared.subject
    else {
        return Err(miette::miette!(
            "audit finalization requires an audit subject"
        ));
    };
    Ok(lk_graph::audit::AuditAssessment {
        path: path.clone(),
        file_revision: file_revision.clone(),
        input_revision: input_revision.clone(),
        disposition: lk_graph::audit::AuditDisposition::Reviewed,
        reason: String::new(),
        reviews: serde_json::from_value(prepared.context["purpose"]["review_dependencies"].clone())
            .into_diagnostic()?,
    })
}

pub(super) fn request(
    work: &Work,
    prepared: &Prepared,
    completed: &Value,
) -> miette::Result<Value> {
    if completed["work_id"] != work.plan.id || completed["context"] != prepared.context {
        return Err(miette::miette!(
            "audit finalizer differs from completed Work"
        ));
    }
    Ok(json!({
        "schema":"native-audit-results-v1",
        "subjects":[{"subject_ref":"s0", "work":completed}],
        "max_input_bytes":work.max_input_bytes,
        "max_result_bytes":work.plan.max_result_bytes,
        "max_response_bytes":audit::response_limit(&[assessment(prepared)?], work.plan.max_result_bytes)?,
        "response_schema":audit::schema(),
        "instructions":audit::instructions(),
        "contracts":audit::contracts()?,
    }))
}

pub(super) fn assemble(
    prepared: &Prepared,
    reply: Value,
) -> miette::Result<super::generation::Outcome> {
    match audit::decode(reply, "s0")? {
        Judgment::NeedsAction(reason) => Ok(super::generation::Outcome::NeedsAction(reason)),
        Judgment::Complete {
            disposition,
            reason,
        } => {
            let mut assessment = assessment(prepared)?;
            assessment.disposition = disposition;
            assessment.reason = reason;
            Ok(super::generation::Outcome::Complete(
                json!({"assessments":[assessment]}),
            ))
        }
    }
}

#[derive(Clone)]
pub(in crate::commands) struct WholeSubject {
    prepared: Prepared,
    pub candidate: lk_graph::audit::AuditCandidate,
    revision: String,
    contract_revision: String,
    action: Option<crate::commands::audit::action::Snapshot>,
}

impl WholeSubject {
    fn binding(&self) -> crate::commands::audit::action::Binding<'_> {
        crate::commands::audit::action::Binding {
            path: &self.candidate.path,
            context_revision: &self.revision,
            contract_revision: &self.contract_revision,
        }
    }

    pub fn hold(&self) -> Option<&str> {
        self.action
            .as_ref()
            .and_then(|action| action.reason(&self.binding()))
    }

    pub fn same_input(&self, other: &Self) -> bool {
        self.candidate.path == other.candidate.path
            && self.revision == other.revision
            && self.contract_revision == other.contract_revision
    }

    pub fn guard(&self, mutation: &mut lk_vault::MutationPlan) {
        self.prepared.guard_pages(mutation);
    }

    pub fn assessment(
        &self,
        disposition: lk_graph::audit::AuditDisposition,
        reason: String,
    ) -> miette::Result<lk_graph::audit::AuditAssessment> {
        Ok(lk_graph::audit::AuditAssessment {
            disposition,
            reason,
            ..assessment(&self.prepared)?
        })
    }

    pub fn plan_hold(
        &self,
        root: &std::path::Path,
        reason: String,
        mutation: &mut lk_vault::MutationPlan,
    ) -> miette::Result<()> {
        self.guard(mutation);
        crate::commands::audit::action::plan_hold(
            root,
            self.binding(),
            reason,
            self.action.as_ref(),
            mutation,
        )
    }
}

pub(in crate::commands) fn capture_controlled(
    root: &std::path::Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    read: &lk_vault::ReadControl,
) -> miette::Result<WholeSubject> {
    let prepared = super::resources::audit_candidate_controlled(root, config, candidate, read)?;
    capture_prepared(root, config_revision, candidate, prepared)
}

pub(super) fn capture_prepared(
    root: &std::path::Path,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    prepared: Prepared,
) -> miette::Result<WholeSubject> {
    Ok(WholeSubject {
        revision: prepared.revision(config_revision)?,
        prepared,
        candidate: candidate.clone(),
        contract_revision: contract_revision()?,
        action: crate::commands::audit::action::read(root, &candidate.path)?,
    })
}

fn contract_revision() -> miette::Result<String> {
    Ok(lk_core::rss_original::digest(
        &serde_json::to_vec(&(audit::schema(), audit::instructions(), audit::contracts()?))
            .into_diagnostic()?,
    ))
}

pub(super) fn processing_hold(
    root: &std::path::Path,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    prepared: &Prepared,
) -> miette::Result<Option<String>> {
    let Some(action) = crate::commands::audit::action::read(root, &candidate.path)? else {
        return Ok(None);
    };
    let revision = prepared.revision(config_revision)?;
    let contract = contract_revision()?;
    Ok(action
        .reason(&crate::commands::audit::action::Binding {
            path: &candidate.path,
            context_revision: &revision,
            contract_revision: &contract,
        })
        .map(str::to_owned))
}

pub(in crate::commands) fn whole_request(
    config: &lk_core::config::Config,
    subjects: &[WholeSubject],
) -> miette::Result<Value> {
    let bounds = super::limits(config)?;
    let assessments = subjects
        .iter()
        .map(|subject| assessment(&subject.prepared))
        .collect::<miette::Result<Vec<_>>>()?;
    let mut response_limit = audit::response_limit(&assessments, bounds.max_result_bytes)?;
    for subject in subjects {
        response_limit = response_limit.min(crate::commands::audit::action::response_limit(
            &subject.binding(),
        )?);
    }
    Ok(json!({
        "schema":"native-audit-results-v1",
        "subjects":subjects.iter().enumerate().map(|(index, subject)| json!({
            "subject_ref":format!("s{index}"), "context":subject.prepared.context,
            "resources":subject.prepared.resources,
        })).collect::<Vec<_>>(),
        "max_input_bytes":bounds.max_input_bytes,
        "max_result_bytes":bounds.max_result_bytes,
        "max_response_bytes":response_limit,
        "response_schema":audit::schema(), "instructions":audit::instructions(),
        "contracts":audit::contracts()?,
    }))
}
