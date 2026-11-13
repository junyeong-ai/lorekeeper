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
    read: &ReadControl,
) -> Result<Adoption, GraphError> {
    let (mutation, adoption) = build_adoption(root, dirs, pages, request, read)?;
    mutation.validate_addresses(root).map_err(error)?;
    Ok(adoption)
}

pub fn adopt(
    session: &MutationSession,
    dirs: &lk_core::config::VaultDirs,
    pages: &[crate::scan::ScannedPage],
    request: AdoptionRequest,
    read: &ReadControl,
) -> Result<Adoption, GraphError> {
    let (mutation, adoption) = build_adoption(session.root(), dirs, pages, request, read)?;
    if mutation.has_changes() {
        session.apply(mutation).map_err(error)?;
    }
    Ok(adoption)
}

pub(super) fn build_adoption(
    root: &Path,
    dirs: &lk_core::config::VaultDirs,
    pages: &[crate::scan::ScannedPage],
    request: AdoptionRequest,
    read: &ReadControl,
) -> Result<(MutationPlan, Adoption), GraphError> {
    match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
        Ok(_) => {
            return Err(error(
                "pending mutation journal prevents discovery adoption",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(error(source)),
    }
    let (before, mut receipts) = super::super::load_controlled(root, read)?;
    let selected = BTreeSet::from([request.path.as_str()]);
    let mut candidates = super::super::inputs_selected_controlled(
        root,
        dirs,
        pages,
        &receipts,
        Some(&selected),
        read,
    )?;
    let candidate = candidates
        .pop()
        .ok_or_else(|| error("discovery adoption requires an exact audit subject"))?;
    if request.file_revision != candidate.file_revision
        || request.input_revision != candidate.input_revision
    {
        return Err(GraphError::AuditScreeningChanged);
    }
    let discovery = candidate
        .discovery
        .as_ref()
        .ok_or_else(|| error("discovery adoption requires current recorded search queries"))?;
    let mut adoption = Adoption {
        path: candidate.path.clone(),
        changed: false,
        previous_input_revision: candidate.input_revision.clone(),
        input_revision: candidate.input_revision.clone(),
        previous_context_pages: candidate.related_pages.len(),
        required_context_pages: candidate.related_pages.len(),
        candidates: discovery.matches.len(),
    };
    if discovery.selection.is_some() {
        return Ok((MutationPlan::default(), adoption));
    }
    if receipts
        .entries
        .get(&candidate.path)
        .is_some_and(|receipt| super::super::receipt_is_current(receipt, &candidate))
    {
        return Err(error(
            "current assessments do not require discovery adoption",
        ));
    }
    let mut mutation = MutationPlan::default();
    super::super::guard_candidate_controlled(&mut mutation, root, &candidate, read)?;
    receipts
        .discovery
        .get_mut(&candidate.path)
        .expect("current recorded discovery")
        .selection = Some(SelectionPlan::default());
    let mut candidates = super::super::inputs_selected_controlled(
        root,
        dirs,
        pages,
        &receipts,
        Some(&selected),
        read,
    )?;
    let candidate = candidates.pop().expect("same guarded audit subject");
    capture_cards(root, dirs, &candidate, read)?.guard(root, &mut mutation, read)?;
    adoption.changed = true;
    adoption.input_revision = candidate.input_revision.clone();
    adoption.required_context_pages = candidate.related_pages.len();
    adoption.candidates = candidate
        .discovery
        .as_ref()
        .and_then(|discovery| discovery.selection.as_ref())
        .expect("adopted discovery selection")
        .total;
    mutation
        .publish_expected(
            root,
            super::super::STATE,
            before,
            serde_json::to_string_pretty(&receipts).map_err(error)? + "\n",
        )
        .map_err(error)?;
    Ok((mutation, adoption))
}
