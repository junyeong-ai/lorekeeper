use std::collections::VecDeque;

use super::*;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptionRequest {
    pub path: String,
    pub file_revision: String,
    pub input_revision: String,
}

#[derive(Serialize)]
pub struct Adoption {
    pub path: String,
    pub changed: bool,
    pub previous_input_revision: String,
    pub input_revision: String,
    pub previous_context_pages: usize,
    pub required_context_pages: usize,
    pub candidates: usize,
}

pub fn preview_adoption(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    pages: &[crate::scan::ScannedPage],
    request: AdoptionRequest,
    guard: impl FnOnce(&AuditCandidate) -> Result<(), GraphError>,
    read: &ReadControl,
) -> Result<Adoption, GraphError> {
    let (mutation, adoption) = build_adoption(root, dirs, pages, request, guard, read)?;
    mutation.validate_addresses(root).map_err(error)?;
    Ok(adoption)
}

pub fn adopt(
    session: &MutationSession,
    dirs: &lk_core::config::VaultDirs,
    pages: &[crate::scan::ScannedPage],
    request: AdoptionRequest,
    guard: impl FnOnce(&AuditCandidate) -> Result<(), GraphError>,
    read: &ReadControl,
) -> Result<Adoption, GraphError> {
    let mut adoptions = Adoptions::capture(session.root(), dirs, pages, &[request], read)?;
    adoptions
        .adopt_next(session, guard)
        .expect("one requested subject")
        .1
}

pub(super) fn build_adoption(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    pages: &[crate::scan::ScannedPage],
    request: AdoptionRequest,
    guard: impl FnOnce(&AuditCandidate) -> Result<(), GraphError>,
    read: &ReadControl,
) -> Result<(MutationPlan, Adoption), GraphError> {
    let mut adoptions = Adoptions::capture(root, dirs, pages, &[request], read)?;
    let (path, prepared) = adoptions
        .subjects
        .pop_front()
        .expect("one requested subject");
    let (mutation, adoption, _) = adoptions.plan(root, &path, prepared?, guard)?;
    Ok((mutation, adoption))
}

/// The discovery adoptions of one checkpoint. The requested subjects' current inputs, their
/// inputs once adopted, their screened card rosters and their read guards come from one
/// shared capture; [`Self::adopt_next`] then adopts each alone, in request order, under its
/// own journal, so a caller can stop between commits and leave the rest legacy.
pub struct Adoptions {
    read: ReadControl,
    state: Option<String>,
    receipts: super::super::Receipts,
    subjects: VecDeque<(String, Result<Prepared, GraphError>)>,
}

struct Prepared {
    adoption: Adoption,
    /// The subject's current candidate and the guards of its complete current input and its
    /// adopted card roster; `None` when the subject already screens.
    legacy: Option<(AuditCandidate, MutationPlan)>,
}

impl Adoptions {
    pub fn capture(
        root: &Path,
        dirs: &lk_core::config::VaultDirs,
        pages: &[crate::scan::ScannedPage],
        requests: &[AdoptionRequest],
        read: &ReadControl,
    ) -> Result<Self, GraphError> {
        match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
            Ok(_) => {
                return Err(error(
                    "pending mutation journal prevents discovery adoption",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(error(source)),
        }
        let (state, receipts) = super::super::load_controlled(root, read)?;
        let mut adoptions = Self {
            read: read.clone(),
            state,
            receipts,
            subjects: VecDeque::new(),
        };
        let scope = super::super::Scope::Selection(
            requests
                .iter()
                .map(|request| request.path.as_str())
                .collect(),
        );
        let evidence = crate::backlinks::concept_evidence_controlled(pages, root, dirs, read)?;
        let derive = |receipts: &super::super::Receipts| {
            super::super::inputs_from_evidence(
                root,
                dirs,
                pages,
                receipts,
                super::super::InputUse::Audit,
                &scope,
                &evidence,
                read,
            )
            .map(|candidates| {
                candidates
                    .into_iter()
                    .map(|candidate| (candidate.path.clone(), candidate))
                    .collect::<BTreeMap<_, _>>()
            })
        };
        let mut current = derive(&adoptions.receipts)?;
        let mut admitted = Vec::with_capacity(requests.len());
        for request in requests {
            let candidate = current
                .remove(&request.path)
                .ok_or_else(|| error("discovery adoption requires an exact audit subject"))?;
            admitted.push((request.path.clone(), admit(request, candidate)));
        }
        // Every legacy subject's adopted inputs come from one more derivation.
        let legacy = admitted
            .iter()
            .filter(|(_, admitted)| matches!(admitted, Ok((_, Some(_)))))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in &legacy {
            adoptions.set_selection(path, Some(SelectionPlan::default()));
        }
        let adopted = derive(&adoptions.receipts);
        for path in &legacy {
            adoptions.set_selection(path, None);
        }
        let mut adopted = adopted?;
        let adopted = legacy
            .iter()
            .map(|path| adopted.remove(path).expect("same guarded audit subject"))
            .collect::<Vec<_>>();
        let cards = capture_card_batch(root, dirs, &adopted.iter().collect::<Vec<_>>(), read)?;
        let mut adopted = adopted.into_iter().zip(cards);
        for (path, admitted) in admitted {
            let prepared = match admitted {
                Ok((adoption, None)) => Ok(Prepared {
                    adoption,
                    legacy: None,
                }),
                Ok((mut adoption, Some(current))) => {
                    let (candidate, cards) = adopted.next().expect("an adopted legacy subject");
                    cards.and_then(|cards| {
                        let mut guards = MutationPlan::default();
                        super::super::guard_candidate_controlled(
                            &mut guards,
                            root,
                            &current,
                            read,
                        )?;
                        cards.guard(root, &mut guards, read)?;
                        adoption.changed = true;
                        adoption.input_revision = candidate.input_revision.clone();
                        adoption.required_context_pages = candidate.related_pages.len();
                        adoption.candidates = candidate
                            .discovery
                            .as_ref()
                            .and_then(|discovery| discovery.selection.as_ref())
                            .expect("adopted discovery selection")
                            .total;
                        Ok(Prepared {
                            adoption,
                            legacy: Some((current, guards)),
                        })
                    })
                }
                Err(refusal) => Err(refusal),
            };
            adoptions.subjects.push_back((path, prepared));
        }
        Ok(adoptions)
    }

    /// Adopts the next requested subject through `session`, once `guard` accepts its current
    /// processing state, and journals that subject alone. `None` once every requested subject
    /// was answered. A guarded byte that changed after the shared capture refuses at
    /// preflight, before anything is journaled: that subject's inputs changed like any
    /// planning drift (`AuditScreeningChanged`).
    pub fn adopt_next(
        &mut self,
        session: &MutationSession,
        guard: impl FnOnce(&AuditCandidate) -> Result<(), GraphError>,
    ) -> Option<(String, Result<Adoption, GraphError>)> {
        let (path, prepared) = self.subjects.pop_front()?;
        let result = prepared.and_then(|prepared| self.commit(session, &path, prepared, guard));
        Some((path, result))
    }

    fn commit(
        &mut self,
        session: &MutationSession,
        path: &str,
        prepared: Prepared,
        guard: impl FnOnce(&AuditCandidate) -> Result<(), GraphError>,
    ) -> Result<Adoption, GraphError> {
        let (mutation, adoption, state) = self.plan(session.root(), path, prepared, guard)?;
        let Some(state) = state else {
            return Ok(adoption);
        };
        self.read.check().map_err(GraphError::from)?;
        session.apply(mutation).map_err(|failure| match failure {
            lk_vault::VaultError::MutationConflict(_) => GraphError::AuditScreeningChanged,
            failure => error(failure),
        })?;
        self.set_selection(path, Some(SelectionPlan::default()));
        self.state = Some(state);
        Ok(adoption)
    }

    /// The subject's adoption plan: its captured guards and the receipt store with its
    /// discovery screened, expecting the store as this batch last left it, and that store's
    /// bytes. A subject that already screens needs no plan.
    fn plan(
        &mut self,
        root: &Path,
        path: &str,
        prepared: Prepared,
        guard: impl FnOnce(&AuditCandidate) -> Result<(), GraphError>,
    ) -> Result<(MutationPlan, Adoption, Option<String>), GraphError> {
        let Some((current, mut mutation)) = prepared.legacy else {
            return Ok((MutationPlan::default(), prepared.adoption, None));
        };
        guard(&current)?;
        self.set_selection(path, Some(SelectionPlan::default()));
        let state = serde_json::to_string_pretty(&self.receipts).map_err(error);
        self.set_selection(path, None);
        let state = state? + "\n";
        mutation
            .publish_expected(root, super::super::STATE, self.state.clone(), state.clone())
            .map_err(error)?;
        Ok((mutation, prepared.adoption, Some(state)))
    }

    fn set_selection(&mut self, path: &str, selection: Option<SelectionPlan>) {
        self.receipts
            .discovery
            .get_mut(path)
            .expect("current recorded discovery")
            .selection = selection;
    }
}

/// The adoption a requested subject's current candidate admits: unchanged when it already
/// screens, or its legacy candidate to adopt. A changed subject or a current assessment
/// refuses.
fn admit(
    request: &AdoptionRequest,
    candidate: AuditCandidate,
) -> Result<(Adoption, Option<AuditCandidate>), GraphError> {
    if request.file_revision != candidate.file_revision
        || request.input_revision != candidate.input_revision
    {
        return Err(GraphError::AuditScreeningChanged);
    }
    let discovery = candidate
        .discovery
        .as_ref()
        .ok_or_else(|| error("discovery adoption requires current recorded search queries"))?;
    let adoption = Adoption {
        path: candidate.path.clone(),
        changed: false,
        previous_input_revision: candidate.input_revision.clone(),
        input_revision: candidate.input_revision.clone(),
        previous_context_pages: candidate.related_pages.len(),
        required_context_pages: candidate.related_pages.len(),
        candidates: discovery.matches.len(),
    };
    if discovery.selection.is_some() {
        return Ok((adoption, None));
    }
    if candidate.reason == super::super::AuditReason::Current {
        return Err(GraphError::AuditSubjectCurrent);
    }
    Ok((adoption, Some(candidate)))
}
