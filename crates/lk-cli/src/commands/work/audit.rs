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

/// Compact JSON bytes an assessment request gives one subject's context cards.
const CONTEXT_CARD_BYTES: usize = 2048;

const CONTEXT_CARD_AUTHORITY: &str = "These pages matched the discovery queries beyond the pages read in full. Only their address and title are shown: they were not read, support nothing and settle nothing.";

/// Shows subject `index` its context cards in `request`, in roster order while they fit
/// `CONTEXT_CARD_BYTES` and the complete request fits `max_input_bytes`, and counts the rest
/// as omitted. The request already holds the subject's evidence, so cards only take the room
/// it leaves; without room even for the count they are not shown.
fn disclose_context_cards(
    request: &mut Value,
    index: usize,
    cards: &[lk_graph::audit::AuditContextCard],
    max_input_bytes: usize,
) -> miette::Result<()> {
    if cards.is_empty() {
        return Ok(());
    }
    let disclosure = |shown: usize| {
        json!({"authority":CONTEXT_CARD_AUTHORITY, "pages":&cards[..shown],
            "omitted":cards.len() - shown})
    };
    let fits = |request: &mut Value, shown: usize| -> miette::Result<bool> {
        if serde_json::to_vec(&cards[..shown]).into_diagnostic()?.len() > CONTEXT_CARD_BYTES {
            return Ok(false);
        }
        request["subjects"][index]["context_cards"] = disclosure(shown);
        Ok(serde_json::to_vec(request).into_diagnostic()?.len() <= max_input_bytes)
    };
    // The largest fitting prefix: fitting is monotone in the number shown.
    let (mut low, mut high) = (0, cards.len());
    if !fits(request, low)? {
        request["subjects"][index]
            .as_object_mut()
            .expect("an assessment subject is an object")
            .remove("context_cards");
        return Ok(());
    }
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if fits(request, middle)? {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    request["subjects"][index]["context_cards"] = disclosure(low);
    Ok(())
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
    let mut request = json!({
        "schema":"native-audit-results-v1",
        "subjects":[{"subject_ref":"s0", "work":completed}],
        "max_input_bytes":work.max_input_bytes,
        "max_result_bytes":work.plan.max_result_bytes,
        "max_response_bytes":audit::response_limit(&[assessment(prepared)?], work.plan.max_result_bytes)?,
        "response_schema":audit::schema(),
        "instructions":audit::instructions(),
        "contracts":audit::contracts()?,
        "contract_revision":contract_revision()?,
    });
    disclose_context_cards(
        &mut request,
        0,
        &prepared.audit_context_cards,
        work.max_input_bytes,
    )?;
    Ok(request)
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
            let result = json!({"assessments":[assessment]});
            match prepared.validate_audit_result(&result) {
                Ok(_) => Ok(super::generation::Outcome::Complete(result)),
                Err(error) => match crate::commands::audit::assessment_blocker(&error) {
                    Some(reason) => Ok(super::generation::Outcome::NeedsAction(reason)),
                    None => Err(error),
                },
            }
        }
    }
}

/// The audit-assessment attempt binding for this prepared subject and its context revision.
pub(super) fn assessment_binding(
    prepared: &Prepared,
    context_revision: &str,
    request: &Value,
) -> miette::Result<super::preparation::Binding> {
    let Subject::Audit {
        path,
        file_revision,
        ..
    } = &prepared.subject
    else {
        return Err(miette::miette!(
            "audit assessment requires an audit subject"
        ));
    };
    super::preparation::Binding::audit_assessment(
        path,
        file_revision,
        context_revision,
        request,
        &audit::schema(),
    )
}

#[derive(Clone)]
pub(in crate::commands) struct WholeSubject {
    prepared: Prepared,
    pub candidate: lk_graph::audit::AuditCandidate,
    revision: String,
    /// The stored Work that continues this subject, which a whole judgment cannot complete.
    stored_work: Option<String>,
    contract_revision: String,
    action: Option<crate::commands::audit::action::Snapshot>,
    attempt: Option<String>,
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
            .or(self.attempt.as_deref())
    }

    pub fn same_input(&self, other: &Self) -> bool {
        self.candidate.path == other.candidate.path
            && self.revision == other.revision
            && self.stored_work == other.stored_work
            && self.contract_revision == other.contract_revision
    }

    pub fn guard(&self, mutation: &mut lk_vault::MutationPlan) {
        self.prepared.guard_pages(mutation);
    }

    /// Guards this subject's inputs for its published assessment and retires the attempt
    /// retained for the subject.
    pub fn plan_publication(
        &self,
        root: &std::path::Path,
        mutation: &mut lk_vault::MutationPlan,
    ) -> miette::Result<()> {
        self.guard(mutation);
        super::preparation::plan_retire(
            root,
            &super::preparation::Key::AuditAssessment {
                path: self.candidate.path.clone(),
            },
            mutation,
        )
    }

    pub fn assessment(
        &self,
        disposition: lk_graph::audit::AuditDisposition,
        reason: String,
    ) -> miette::Result<lk_graph::audit::AuditAssessment> {
        let assessment = lk_graph::audit::AuditAssessment {
            disposition,
            reason,
            ..assessment(&self.prepared)?
        };
        self.prepared
            .validate_audit_result(&json!({"assessments":[assessment]}))
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

    /// Holds this exact subject input after its tool-enabled `request` failed to return; the
    /// returned binding reports the hold once `mutation` is applied.
    pub fn plan_failure_hold(
        &self,
        root: &std::path::Path,
        request: &Value,
        failure: super::state::InvocationFailure,
        mutation: &mut lk_vault::MutationPlan,
    ) -> miette::Result<super::preparation::Binding> {
        self.guard(mutation);
        let binding = assessment_binding(&self.prepared, &self.revision, request)?;
        mutation.extend(binding.plan_record(
            root,
            super::preparation::Disposition::invocation_failed(failure),
        )?);
        Ok(binding)
    }
}

pub(in crate::commands) fn capture_controlled(
    root: &std::path::Path,
    config: &lk_core::config::Config,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    read: &lk_vault::ReadControl,
) -> miette::Result<WholeSubject> {
    let mut prepared = super::resources::audit_candidate_controlled(root, config, candidate, read)?;
    let work = super::expected(root, &mut prepared, config, config_revision)?;
    capture_prepared(root, config_revision, candidate, prepared, &work)
}

/// `prepared` as a whole subject; `work` is the Work it continues (`expected()`).
pub(super) fn capture_prepared(
    root: &std::path::Path,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    prepared: Prepared,
    work: &super::Work,
) -> miette::Result<WholeSubject> {
    let revision = prepared.revision(config_revision)?;
    Ok(WholeSubject {
        attempt: assessment_binding(&prepared, &revision, &json!({}))?.held(root)?,
        revision,
        stored_work: work.is_planned(root)?.then(|| work.plan.id.clone()),
        prepared,
        candidate: candidate.clone(),
        contract_revision: contract_revision()?,
        action: crate::commands::audit::action::read(root, &candidate.path)?,
    })
}

fn contract_revision() -> miette::Result<String> {
    let dependencies =
        lk_dist::skill_files_for(lk_core::execution::AgentEngine::Codex, "lore-wiki")
            .into_iter()
            .filter(|file| {
                matches!(
                    file.relative,
                    "references/maintenance.md" | "references/curation.md"
                )
            })
            .map(|file| (file.relative, file.contents))
            .collect::<Vec<_>>();
    if dependencies.len() != 2 {
        return Err(miette::miette!(
            "native audit requires complete maintenance and curation dependencies"
        ));
    }
    hash_contracts(&dependencies, &audit::contracts()?)
}

fn hash_contracts(dependencies: &[(&str, &str)], supplied: &[Value]) -> miette::Result<String> {
    Ok(lk_core::rss_original::digest(
        &serde_json::to_vec(&(
            audit::schema(),
            audit::instructions(),
            dependencies,
            supplied,
        ))
        .into_diagnostic()?,
    ))
}

pub(super) fn processing_hold(
    root: &std::path::Path,
    config_revision: &str,
    candidate: &lk_graph::audit::AuditCandidate,
    prepared: &Prepared,
) -> miette::Result<Option<String>> {
    let revision = prepared.revision(config_revision)?;
    if let Some(action) = crate::commands::audit::action::read(root, &candidate.path)?
        && let Some(reason) = action.reason(&crate::commands::audit::action::Binding {
            path: &candidate.path,
            context_revision: &revision,
            contract_revision: &contract_revision()?,
        })
    {
        return Ok(Some(reason.to_owned()));
    }
    assessment_binding(prepared, &revision, &json!({}))?.held(root)
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
    let mut request = json!({
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
        "contract_revision":contract_revision()?,
    });
    for (index, subject) in subjects.iter().enumerate() {
        disclose_context_cards(
            &mut request,
            index,
            &subject.prepared.audit_context_cards,
            bounds.max_input_bytes,
        )?;
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_cards(count: usize) -> Vec<lk_graph::audit::AuditContextCard> {
        (0..count)
            .map(|index| lk_graph::audit::AuditContextCard {
                path: format!("wiki/documents/match-{index:02}.md"),
                title: format!("Match {index} {}", "with a long declared title ".repeat(4)),
            })
            .collect()
    }

    #[test]
    fn context_cards_take_only_the_room_their_allowance_and_the_request_leave() {
        let evidence = json!({"subjects":[{"subject_ref":"s0", "resources":["evidence"]}]});
        let cards = context_cards(40);
        let shown = |request: &Value| {
            request["subjects"][0]["context_cards"]["pages"]
                .as_array()
                .unwrap()
                .len()
        };
        let bytes = |value: &Value| serde_json::to_vec(value).unwrap().len();

        let mut request = evidence.clone();
        disclose_context_cards(&mut request, 0, &cards, usize::MAX).unwrap();
        let allowed = shown(&request);
        assert!(bytes(&request["subjects"][0]["context_cards"]["pages"]) <= CONTEXT_CARD_BYTES);
        assert!(serde_json::to_vec(&cards[..=allowed]).unwrap().len() > CONTEXT_CARD_BYTES);
        assert_eq!(
            request["subjects"][0]["context_cards"]["omitted"],
            cards.len() - allowed
        );

        // Near the input bound the cards give way, and the request still fits.
        let bound = bytes(&request) - 1;
        let mut tight = evidence.clone();
        disclose_context_cards(&mut tight, 0, &cards, bound).unwrap();
        assert!(bytes(&tight) <= bound);
        assert!(shown(&tight) < allowed);
        assert_eq!(
            tight["subjects"][0]["context_cards"]["omitted"],
            cards.len() - shown(&tight)
        );

        // Without room even for the count, the evidence stands alone.
        let mut full = evidence.clone();
        disclose_context_cards(&mut full, 0, &cards, bytes(&evidence)).unwrap();
        assert_eq!(full, evidence);
    }

    #[test]
    fn repair_procedure_changes_reopen_the_contract_without_expanding_its_payload() {
        let supplied = audit::contracts().unwrap();
        let dependency =
            "## Curation criteria\nPreserve evidence.\n## Repair\nRead the original.\n";
        let changed = dependency.replace("Read the original.", "Read both guarded revisions.");
        assert_ne!(
            hash_contracts(&[("curation.md", dependency)], &supplied).unwrap(),
            hash_contracts(&[("curation.md", &changed)], &supplied).unwrap(),
        );
    }
}
