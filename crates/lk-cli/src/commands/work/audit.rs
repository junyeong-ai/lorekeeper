use miette::IntoDiagnostic;
use serde_json::{Value, json};

use super::{Prepared, Subject, Work};
use crate::commands::execution::audit::{self, Judgment};

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
        "subjects":[{"work":completed}],
        "max_input_bytes":work.max_input_bytes,
        "max_result_bytes":work.plan.max_result_bytes,
        "max_response_bytes":audit::response_limit(&assessment(prepared)?, work.plan.max_result_bytes)?,
        "response_schema":audit::schema(),
        "instructions":audit::instructions(),
    }))
}

pub(super) fn assemble(
    prepared: &Prepared,
    reply: Value,
) -> miette::Result<super::generation::Outcome> {
    match audit::decode(reply)? {
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
