//! Incremental semantic-review receipts. They track work, never establish factual truth.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use lk_core::concept::{Citation, file_revision, source_revision};
use lk_core::config::VaultDirs;
use lk_core::frontmatter::KnowledgeStatus;
use lk_vault::{MutationPlan, MutationSession};
use serde::{Deserialize, Serialize};

use crate::{GraphError, scan::ScannedPage};

const STATE: &str = ".lorekeeper/knowledge-audit.json";
const POLICY: &str = "concept-audit-v1";

fn error(message: impl std::fmt::Display) -> GraphError {
    GraphError::Io(message.to_string())
}

#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuditDisposition {
    Reviewed,
    FollowUp,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConceptAssessment {
    pub path: String,
    pub file_revision: String,
    pub input_revision: String,
    pub disposition: AuditDisposition,
    pub reason: String,
    #[serde(default)]
    pub reviews: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditResults {
    pub assessments: Vec<ConceptAssessment>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipts {
    entries: BTreeMap<String, ConceptAssessment>,
    pending: Vec<String>,
    next_class: AuditClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_merges: Option<WorkflowMerges>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowMerges {
    workflow_id: lk_core::execution::WorkflowId,
    concepts: BTreeMap<String, ConceptRetirement>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConceptRetirement {
    into: String,
    from_revision: String,
}

#[derive(Serialize)]
pub struct AuditCompletion {
    pub requested_path: String,
    pub assessed_path: String,
    pub disposition: AuditDisposition,
}

pub(crate) fn record_merge(
    plan: &mut MutationPlan,
    root: &Path,
    workflow_id: &lk_core::execution::WorkflowId,
    from: &Path,
    original: &str,
    into: &Path,
) -> Result<(), GraphError> {
    let (before, mut receipts) = load(root)?;
    if receipts
        .last_merges
        .as_ref()
        .is_none_or(|merges| merges.workflow_id != *workflow_id)
    {
        receipts.last_merges = Some(WorkflowMerges {
            workflow_id: workflow_id.clone(),
            concepts: BTreeMap::new(),
        });
    }
    receipts
        .last_merges
        .as_mut()
        .expect("initialized merge context")
        .concepts
        .insert(
            from.to_string_lossy().into_owned(),
            ConceptRetirement {
                into: into.to_string_lossy().into_owned(),
                from_revision: file_revision(original),
            },
        );
    let after = serde_json::to_string_pretty(&receipts).map_err(error)? + "\n";
    plan.publish_expected(root, STATE, before, after)
        .map_err(error)
}

/// Verify original batch slots through current assessments or native guarded merges.
/// The operation identifier correlates receipts; it never authorizes a lock bypass.
pub fn verify_batch(
    root: &Path,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
    batch: &[AuditCandidate],
    workflow_id: &lk_core::execution::WorkflowId,
    max_input_bytes: u64,
) -> Result<Vec<AuditCompletion>, GraphError> {
    let (_, receipts) = load(root)?;
    let candidates = inputs(root, dirs, pages)?
        .into_iter()
        .map(|candidate| (candidate.path.clone(), candidate))
        .collect::<BTreeMap<_, _>>();
    let reviews = lk_vault::list_reviews(root, dirs).map_err(error)?;
    let merges = receipts
        .last_merges
        .as_ref()
        .filter(|merges| merges.workflow_id == *workflow_id);
    let mut verified = Vec::new();
    let mut assessed = BTreeMap::new();
    for original in batch {
        let mut path = original.path.as_str();
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(path) {
                return Err(error("audit merge chain contains a cycle"));
            }
            if let Some(candidate) = candidates.get(path) {
                let receipt = receipts
                    .entries
                    .get(path)
                    .filter(|receipt| receipt_is_current(receipt, candidate, &reviews))
                    .ok_or_else(|| {
                        error(format!("audit concept lacks a current assessment: {path}"))
                    })?;
                verified.push(AuditCompletion {
                    requested_path: original.path.clone(),
                    assessed_path: path.into(),
                    disposition: receipt.disposition,
                });
                assessed.insert(candidate.path.clone(), candidate.clone());
                break;
            }
            match std::fs::symlink_metadata(
                lk_vault::page_path(root, Path::new(path)).map_err(error)?,
            ) {
                Ok(_) => {
                    return Err(error(format!(
                        "audit concept is no longer a valid concept: {path}"
                    )));
                }
                Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
                Err(failure) => return Err(error(failure)),
            }
            let retirement = merges
                .and_then(|merges| merges.concepts.get(path))
                .ok_or_else(|| {
                    error(format!(
                        "audit concept disappeared without this workflow's guarded merge: {path}"
                    ))
                })?;
            path = &retirement.into;
        }
    }
    measure_inputs(root, assessed.values(), max_input_bytes)?.ensure_ready()?;
    Ok(verified)
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum AuditClass {
    #[default]
    Changed,
    Unreviewed,
}

#[derive(Clone, Serialize)]
pub struct AuditCandidate {
    pub path: String,
    pub title: String,
    pub file_revision: String,
    pub input_revision: String,
    pub knowledge_status: KnowledgeStatus,
    pub evidence: Vec<Citation>,
    pub related_pages: Vec<Citation>,
    pub reason: &'static str,
}

#[derive(Serialize)]
pub struct AuditPlan {
    pub candidates: Vec<AuditCandidate>,
    pub total: usize,
    pub reviewed_current: usize,
    pub follow_up_current: usize,
    pub unreviewed: usize,
    pub changed: usize,
    pub limit: usize,
    pub input: AuditInputBound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditInputStatus {
    Ready,
    Oversized,
}

/// Complete distinct source footprint, not a bound on model tokens, calls or cost.
/// Pages include the selected concepts, direct evidence and directly related concepts;
/// RSS payloads include the exact immutable selected JSON, in addition to origin pages.
#[derive(Serialize)]
pub struct AuditInputBound {
    pub status: AuditInputStatus,
    pub bytes: u64,
    pub max_bytes: u64,
    pub pages: usize,
    pub rss_payloads: usize,
}

impl AuditInputBound {
    pub fn ensure_ready(&self) -> Result<(), GraphError> {
        if self.status == AuditInputStatus::Oversized {
            return Err(error(format!(
                "audit input is oversized: {} source bytes exceeds graph.audit.max_input_bytes {}; selected work remains pending",
                self.bytes, self.max_bytes,
            )));
        }
        Ok(())
    }
}

#[derive(Default)]
struct InputResources {
    pages: BTreeMap<PathBuf, (String, u64)>,
    rss_payloads: BTreeMap<(String, String), u64>,
}

impl InputResources {
    fn page(
        &mut self,
        root: &Path,
        path: &str,
        expected_file: Option<&str>,
        expected_source: Option<&str>,
    ) -> Result<(), GraphError> {
        let absolute = lk_vault::page_path(root, Path::new(path)).map_err(error)?;
        let canonical = absolute.canonicalize().map_err(error)?;
        let raw = std::fs::read_to_string(&absolute).map_err(error)?;
        let revision = file_revision(&raw);
        let source_matches = match expected_source {
            Some(expected) => {
                source_revision(&lk_vault::parse_page(&raw).map_err(error)?) == expected
            }
            None => true,
        };
        if expected_file.is_some_and(|expected| expected != revision)
            || !source_matches
            || self
                .pages
                .get(&canonical)
                .is_some_and(|(prior, _)| prior != &revision)
        {
            return Err(error(format!(
                "audit dependency changed during input measurement: {path}"
            )));
        }
        self.pages.insert(canonical, (revision, raw.len() as u64));
        Ok(())
    }

    fn rss_payload(
        &mut self,
        root: &Path,
        concept: &str,
        citation: &Citation,
    ) -> Result<(), GraphError> {
        lk_vault::validate_rss_citation(root, Path::new(concept), citation).map_err(error)?;
        let selector = citation.rss_event.as_ref().expect("validated RSS citation");
        let relative = format!(
            "{}/events/{}.json",
            lk_core::rss_original::RSS_ORIGINAL_STORE,
            citation.revision,
        );
        let bytes = std::fs::read(lk_vault::page_path(root, Path::new(&relative)).map_err(error)?)
            .map_err(error)?;
        if lk_core::rss_original::digest(&bytes) != citation.revision {
            return Err(error("RSS payload changed during audit input measurement"));
        }
        self.rss_payloads.insert(
            (selector.event_id.clone(), citation.revision.clone()),
            bytes.len() as u64,
        );
        Ok(())
    }
}

fn measure_inputs<'a>(
    root: &Path,
    candidates: impl IntoIterator<Item = &'a AuditCandidate>,
    max_input_bytes: u64,
) -> Result<AuditInputBound, GraphError> {
    if max_input_bytes == 0 {
        return Err(error("audit max_input_bytes must be positive"));
    }
    let mut resources = InputResources::default();
    for candidate in candidates {
        resources.page(root, &candidate.path, Some(&candidate.file_revision), None)?;
        for citation in candidate.evidence.iter().chain(&candidate.related_pages) {
            if citation.rss_event.is_some() {
                resources.page(root, &citation.path, None, None)?;
                resources.rss_payload(root, &candidate.path, citation)?;
            } else {
                resources.page(root, &citation.path, None, Some(&citation.revision))?;
            }
        }
    }
    let bytes = resources
        .pages
        .values()
        .map(|(_, bytes)| *bytes)
        .chain(resources.rss_payloads.values().copied())
        .try_fold(0u64, |total, bytes| total.checked_add(bytes))
        .ok_or_else(|| error("audit input byte count overflow"))?;
    Ok(AuditInputBound {
        status: if bytes > max_input_bytes {
            AuditInputStatus::Oversized
        } else {
            AuditInputStatus::Ready
        },
        bytes,
        max_bytes: max_input_bytes,
        pages: resources.pages.len(),
        rss_payloads: resources.rss_payloads.len(),
    })
}

fn load(root: &Path) -> Result<(Option<String>, Receipts), GraphError> {
    let path = lk_vault::page_path(root, Path::new(STATE)).map_err(error)?;
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let state = serde_json::from_str(&raw).map_err(error)?;
            Ok((Some(raw), state))
        }
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {
            Ok((None, Receipts::default()))
        }
        Err(failure) => Err(error(failure)),
    }
}

fn inputs(
    root: &Path,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
) -> Result<Vec<AuditCandidate>, GraphError> {
    let evidence = crate::backlinks::concept_evidence(pages, root, dirs)?;
    let by_id: BTreeMap<_, _> = pages.iter().map(|page| (page.id.as_str(), page)).collect();
    let existence = crate::scan::VaultExistence::build_at(root, pages, dirs)?;
    let mut result = Vec::new();
    for page in pages
        .iter()
        .filter(|page| evidence.contains_key(&page.path))
    {
        let raw = std::fs::read_to_string(lk_vault::page_path(root, &page.path).map_err(error)?)
            .map_err(error)?;
        let parsed = lk_vault::parse_page(&raw).map_err(error)?;
        let revision = source_revision(&parsed);
        if page.revision.as_deref() != Some(&revision) {
            return Err(error("concept changed during audit planning"));
        }
        let mut related_pages = Vec::new();
        for related in page
            .outgoing
            .iter()
            .filter_map(|link| existence.reached(link))
            .filter_map(|id| by_id.get(id))
        {
            if related.path != page.path && evidence.contains_key(&related.path) {
                related_pages.push(Citation::at_revision(
                    &related.path,
                    related
                        .revision
                        .clone()
                        .ok_or_else(|| error("unreadable related concept"))?,
                ));
            }
        }
        related_pages.sort();
        related_pages.dedup();
        let citations = evidence[&page.path].clone();
        let input_revision = file_revision(
            &serde_json::to_string(&(POLICY, &revision, &citations, &related_pages))
                .map_err(error)?,
        );
        result.push(AuditCandidate {
            path: page.path.to_string_lossy().into_owned(),
            title: page.title.clone(),
            file_revision: file_revision(&raw),
            input_revision,
            knowledge_status: parsed.frontmatter.knowledge_status().map_err(error)?,
            evidence: citations,
            related_pages,
            reason: "unreviewed",
        });
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn review_assesses(review: &lk_vault::ReviewReport, path: &str) -> bool {
    !matches!(
        review.effective_state,
        lk_vault::ReviewEffectiveState::Invalid | lk_vault::ReviewEffectiveState::NeedsReassessment
    ) && review
        .review
        .as_ref()
        .is_some_and(|metadata| metadata.subjects.iter().any(|subject| subject == path))
}

fn receipt_is_current(
    receipt: &ConceptAssessment,
    candidate: &AuditCandidate,
    reviews: &[lk_vault::ReviewReport],
) -> bool {
    receipt.input_revision == candidate.input_revision
        && (receipt.disposition != AuditDisposition::FollowUp || !receipt.reviews.is_empty())
        && receipt.reviews.iter().all(|path| {
            reviews
                .iter()
                .any(|review| review.path == *path && review_assesses(review, &candidate.path))
        })
}

fn pending(
    candidates: Vec<AuditCandidate>,
    receipts: &Receipts,
    reviews: &[lk_vault::ReviewReport],
) -> (Vec<AuditCandidate>, usize, usize) {
    let mut remaining = BTreeMap::new();
    let mut reviewed_current = 0;
    let mut follow_up_current = 0;
    for mut candidate in candidates {
        match receipts.entries.get(&candidate.path) {
            Some(receipt) if receipt_is_current(receipt, &candidate, reviews) => {
                reviewed_current += 1;
                if receipt.disposition == AuditDisposition::FollowUp {
                    follow_up_current += 1;
                }
            }
            previous => {
                if previous.is_some() {
                    candidate.reason = "changed";
                }
                remaining.insert(candidate.path.clone(), candidate);
            }
        }
    }
    let mut work = Vec::new();
    for path in &receipts.pending {
        if let Some(candidate) = remaining.remove(path) {
            work.push(candidate);
        }
    }
    work.extend(remaining.into_values());
    (work, reviewed_current, follow_up_current)
}

/// Rotate changed and never-reviewed work, retaining first-discovery order in each class.
/// Unchanged inputs skip repeated reads; missing or stale finding reviews reopen work.
pub fn plan(
    root: &Path,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
    limit: usize,
    max_input_bytes: u64,
) -> Result<AuditPlan, GraphError> {
    plan_selected(root, dirs, pages, limit, &[], max_input_bytes)
}

/// Re-read exact batch members after repairs without expanding semantic work scope.
pub fn plan_selected(
    root: &Path,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
    limit: usize,
    selection: &[String],
    max_input_bytes: u64,
) -> Result<AuditPlan, GraphError> {
    if !(1..=100).contains(&limit) {
        return Err(error("audit limit must be 1..100"));
    }
    let (_, receipts) = load(root)?;
    let candidates = inputs(root, dirs, pages)?;
    let total = candidates.len();
    let reviews = lk_vault::list_reviews(root, dirs).map_err(error)?;
    let mut selected = Vec::new();
    let unique: BTreeSet<_> = selection.iter().collect();
    if unique.len() != selection.len() || selection.len() > limit {
        return Err(error(
            "audit selection requires distinct paths within the batch limit",
        ));
    }
    for path in selection {
        let mut candidate = candidates
            .iter()
            .find(|candidate| candidate.path == *path)
            .ok_or_else(|| error("audit selection is not an exact concept address"))?
            .clone();
        candidate.reason = match receipts.entries.get(path) {
            Some(receipt) if receipt_is_current(receipt, &candidate, &reviews) => "current",
            Some(_) => "changed",
            None => "unreviewed",
        };
        selected.push(candidate);
    }
    let (pending, reviewed_current, follow_up_current) = pending(candidates, &receipts, &reviews);
    let (mut changed, mut unreviewed): (VecDeque<_>, VecDeque<_>) = pending
        .into_iter()
        .partition(|candidate| candidate.reason == "changed");
    let changed_count = changed.len();
    let unreviewed_count = unreviewed.len();
    let mut candidates = Vec::new();
    let mut next_class = receipts.next_class;
    while candidates.len() < limit {
        let next = match next_class {
            AuditClass::Changed => changed.pop_front().or_else(|| unreviewed.pop_front()),
            AuditClass::Unreviewed => unreviewed.pop_front().or_else(|| changed.pop_front()),
        };
        let Some(candidate) = next else { break };
        next_class = if candidate.reason == "changed" {
            AuditClass::Unreviewed
        } else {
            AuditClass::Changed
        };
        candidates.push(candidate);
    }
    if !selection.is_empty() {
        candidates = selected;
    }
    let input = measure_inputs(root, &candidates, max_input_bytes)?;
    Ok(AuditPlan {
        candidates,
        total,
        reviewed_current,
        follow_up_current,
        unreviewed: unreviewed_count,
        changed: changed_count,
        limit,
        input,
    })
}

/// Record a completed read with current concept/evidence/related-page guards.
/// Findings remain in authored exploration reviews, not in this operational cache.
pub fn record(
    session: &MutationSession,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
    results: AuditResults,
    max_input_bytes: u64,
) -> Result<Vec<String>, GraphError> {
    if results.assessments.is_empty() {
        return Err(error("audit results must contain assessments"));
    }
    let root = session.root();
    let (before, mut receipts) = load(root)?;
    let candidates = inputs(root, dirs, pages)?;
    let reviews = lk_vault::list_reviews(root, dirs).map_err(error)?;
    let (work, _, _) = pending(candidates.clone(), &receipts, &reviews);
    receipts.pending = work.into_iter().map(|candidate| candidate.path).collect();
    let candidates: BTreeMap<_, _> = candidates
        .into_iter()
        .map(|candidate| (candidate.path.clone(), candidate))
        .collect();
    let selected = results
        .assessments
        .iter()
        .map(|assessment| {
            let candidate = candidates
                .get(&assessment.path)
                .ok_or_else(|| error("audit target is not an exact concept address"))?;
            if candidate.file_revision != assessment.file_revision
                || candidate.input_revision != assessment.input_revision
            {
                return Err(error(format!(
                    "audit inputs changed: {}; reread before recording",
                    assessment.path
                )));
            }
            Ok(candidate)
        })
        .collect::<Result<Vec<_>, _>>()?;
    measure_inputs(root, selected, max_input_bytes)?.ensure_ready()?;
    let mut plan = MutationPlan::default();
    let mut recorded = BTreeSet::new();
    for assessment in results.assessments {
        if !recorded.insert(assessment.path.clone()) || assessment.reason.trim().is_empty() {
            return Err(error(
                "audit assessments require distinct paths and a substantive reason",
            ));
        }
        let candidate = candidates
            .get(&assessment.path)
            .ok_or_else(|| error("audit target is not an exact concept address"))?;
        if assessment.disposition == AuditDisposition::FollowUp && assessment.reviews.is_empty() {
            return Err(error("audit follow-up requires a durable scoped review"));
        }
        for review_path in &assessment.reviews {
            let review = reviews
                .iter()
                .find(|review| review.path == *review_path)
                .ok_or_else(|| error("audit review is missing"))?;
            if !review_assesses(review, &assessment.path) {
                return Err(error("audit review must currently assess this concept"));
            }
            let raw = std::fs::read_to_string(
                lk_vault::page_path(root, Path::new(review_path)).map_err(error)?,
            )
            .map_err(error)?;
            plan.require_unchanged(review_path, raw);
        }
        let raw = std::fs::read_to_string(
            lk_vault::page_path(root, Path::new(&candidate.path)).map_err(error)?,
        )
        .map_err(error)?;
        if file_revision(&raw) != candidate.file_revision {
            return Err(error("audit target changed"));
        }
        plan.require_unchanged(&candidate.path, raw);
        for citation in candidate.evidence.iter().chain(&candidate.related_pages) {
            if citation.rss_event.is_some() {
                lk_vault::guard_rss_citation(&mut plan, root, Path::new(&candidate.path), citation)
                    .map_err(error)?;
            } else {
                let raw = std::fs::read_to_string(
                    lk_vault::page_path(root, Path::new(&citation.path)).map_err(error)?,
                )
                .map_err(error)?;
                if source_revision(&lk_vault::parse_page(&raw).map_err(error)?) != citation.revision
                {
                    return Err(error("audit dependency changed"));
                }
                plan.require_unchanged(&citation.path, raw);
            }
        }
        receipts.next_class = if receipts.entries.contains_key(&assessment.path) {
            AuditClass::Unreviewed
        } else {
            AuditClass::Changed
        };
        receipts.entries.insert(assessment.path.clone(), assessment);
    }
    receipts
        .entries
        .retain(|path, _| candidates.contains_key(path));
    receipts.pending.retain(|path| !recorded.contains(path));
    let after = serde_json::to_string_pretty(&receipts).map_err(error)? + "\n";
    plan.publish_expected(root, STATE, before, after)
        .map_err(error)?;
    session.apply(plan).map_err(error)?;
    Ok(recorded.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(root: &Path, path: &str, body: &str) {
        let destination = root.join(path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(destination, body).unwrap();
    }
    fn scan(root: &Path) -> Vec<ScannedPage> {
        crate::scan::scan_vault(root, false).unwrap()
    }
    fn assessment(candidate: &AuditCandidate) -> ConceptAssessment {
        ConceptAssessment {path:candidate.path.clone(),file_revision:candidate.file_revision.clone(),input_revision:candidate.input_revision.clone(),disposition:AuditDisposition::Reviewed,reason:"Read the complete source and concept; the scoped claim and relationship are supported.".into(),reviews:vec![]}
    }

    #[test]
    fn input_bound_counts_complete_distinct_pages_and_blocks_without_advancing_receipts() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let source = "---\ntype: document\ntitle: 근거\n---\nComplete source body.\n## Related Concepts\n- [A](../concepts/a.md)\n- [B](../concepts/b.md)\n";
        let concept =
            "---\ntype: concept\ntitle: A\n---\n## Synthesis\nA claim.\n## Related\n- [B](b.md)\n";
        let related = "---\ntype: concept\ntitle: B\n---\n## Synthesis\nRelated claim.\n";
        write(root.path(), "wiki/documents/source.md", source);
        write(root.path(), "wiki/concepts/a.md", concept);
        write(root.path(), "wiki/concepts/b.md", related);
        let pages = scan(root.path());
        let expected = (source.len() + concept.len() + related.len()) as u64;
        let ready = plan(root.path(), &dirs, &pages, 2, expected).unwrap();
        assert_eq!(ready.input.bytes, expected);
        assert_eq!(ready.input.pages, 3);
        assert_eq!(ready.input.rss_payloads, 0);
        assert_eq!(ready.input.status, AuditInputStatus::Ready);
        ready.input.ensure_ready().unwrap();
        let oversized = plan(root.path(), &dirs, &pages, 2, expected - 1).unwrap();
        assert_eq!(oversized.input.status, AuditInputStatus::Oversized);
        assert!(oversized.input.ensure_ready().is_err());
        assert_eq!(oversized.candidates.len(), 2);
        assert_eq!(
            oversized
                .candidates
                .iter()
                .map(|value| &value.path)
                .collect::<Vec<_>>(),
            ready
                .candidates
                .iter()
                .map(|value| &value.path)
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            oversized.candidates[0].input_revision, ready.candidates[0].input_revision,
            "changing the resource bound must not invalidate completed semantic receipts",
        );
        let selected = [
            "wiki/concepts/b.md".to_string(),
            "wiki/concepts/a.md".to_string(),
        ];
        let exact = plan_selected(root.path(), &dirs, &pages, 2, &selected, 1).unwrap();
        assert_eq!(exact.candidates[0].path, selected[0]);
        assert_eq!(exact.candidates[1].path, selected[1]);
        assert_eq!(exact.input.bytes, expected);
        assert_eq!(exact.input.status, AuditInputStatus::Oversized);
        let session = MutationSession::acquire(root.path()).unwrap();
        let results = || AuditResults {
            assessments: ready.candidates.iter().map(assessment).collect(),
        };
        let rejected = record(&session, &dirs, &pages, results(), expected - 1).unwrap_err();
        assert!(rejected.to_string().contains("oversized"));
        assert!(!root.path().join(STATE).exists());
        record(&session, &dirs, &pages, results(), expected).unwrap();
        let before = std::fs::read(root.path().join(STATE)).unwrap();
        assert!(record(&session, &dirs, &pages, results(), 1).is_err());
        assert_eq!(std::fs::read(root.path().join(STATE)).unwrap(), before);
        let workflow = lk_core::execution::WorkflowId::from_bytes([1; 16]);
        assert!(verify_batch(root.path(), &dirs, &pages, &ready.candidates, &workflow, 1).is_err());
        verify_batch(
            root.path(),
            &dirs,
            &pages,
            &ready.candidates,
            &workflow,
            expected,
        )
        .unwrap();
        assert!(plan(root.path(), &dirs, &pages, 2, 0).is_err());
    }

    #[test]
    fn input_measurement_refuses_changed_and_missing_dependencies() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let source =
            "---\ntype: document\n---\nSource.\n## Related Concepts\n[A](../concepts/a.md)\n";
        let concept = "---\ntype: concept\ntitle: A\n---\n## Synthesis\nA claim.\n";
        write(root.path(), "wiki/documents/source.md", source);
        write(root.path(), "wiki/concepts/a.md", concept);
        let candidates = inputs(root.path(), &dirs, &scan(root.path())).unwrap();
        write(
            root.path(),
            "wiki/documents/source.md",
            &source.replace("Source.", "Changed."),
        );
        assert!(measure_inputs(root.path(), &candidates, u64::MAX).is_err());
        std::fs::remove_file(root.path().join("wiki/documents/source.md")).unwrap();
        assert!(measure_inputs(root.path(), &candidates, u64::MAX).is_err());
        write(root.path(), "wiki/documents/source.md", source);
        write(
            root.path(),
            "wiki/concepts/a.md",
            &concept.replace("A claim.", "Changed."),
        );
        assert!(measure_inputs(root.path(), &candidates, u64::MAX).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn input_resources_share_canonical_aliases_but_keep_hard_link_owners_distinct() {
        let root = tempfile::tempdir().unwrap();
        let text = "---\ntype: document\n---\nComplete evidence.\n";
        write(root.path(), "source.md", text);
        std::os::unix::fs::symlink("source.md", root.path().join("alias.md")).unwrap();
        std::fs::hard_link(root.path().join("source.md"), root.path().join("hard.md")).unwrap();
        let mut resources = InputResources::default();
        for path in ["source.md", "alias.md", "hard.md"] {
            resources
                .page(root.path(), path, Some(&file_revision(text)), None)
                .unwrap();
        }
        assert_eq!(resources.pages.len(), 2);
        assert_eq!(
            resources
                .pages
                .values()
                .map(|(_, bytes)| bytes)
                .sum::<u64>(),
            2 * text.len() as u64
        );
    }

    #[test]
    fn input_bound_includes_exact_rss_payloads_once_and_rejects_corruption() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let event: lk_core::event::Event = serde_json::from_value(json!({
            "id":"rss:2026-10-18:0000000000000001", "source_id":"rss", "source_type":"rss",
            "timestamp":"2026-10-18T00:00:00Z", "date":"2026-10-18", "title":"보도", "body":"Complete original report.",
            "url":"https://example.test/report", "author":null, "labels":[], "category":null,
            "performance_category":null, "is_self":false, "is_personal":false, "metadata":{}
        })).unwrap();
        let capture = lk_core::rss_original::RssOriginalCapture::from_events(&[&event]).unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let reference = lk_vault::publish_rss_original(&session, &capture).unwrap();
        let selected = &capture.manifest.events[0];
        let paths = ["wiki/concepts/a.md", "wiki/concepts/b.md"];
        let binding = paths
            .iter()
            .map(|concept| {
                json!({
                    "schema":lk_core::rss_evidence::RSS_EVIDENCE_SCHEMA,
                    "concept":concept, "manifest":reference,
                    "events":[{"event_id":selected.event_id, "revision":selected.revision}]
                })
            })
            .collect::<Vec<_>>();
        let source = format!(
            "---\ntype: daily\ntitle: Report\nsource: rss\ncreated: 2026-10-18\nsource_record: {}\nconcept_evidence: {}\n---\n## Related Concepts\n- [A](../wiki/concepts/a.md)\n- [B](../wiki/concepts/b.md)\n",
            json!({"rss_original":reference}),
            json!(binding)
        );
        write(root.path(), "daily/report.md", &source);
        let concept = "---\ntype: concept\ntitle: Subject\n---\n## Synthesis\nScoped claim.\n";
        for path in paths {
            write(root.path(), path, concept);
        }
        let expected = (source.len() + 2 * concept.len() + capture.payloads[0].len()) as u64;
        let input = plan(root.path(), &dirs, &scan(root.path()), 2, expected).unwrap();
        assert_eq!(input.input.status, AuditInputStatus::Ready);
        assert_eq!(input.input.bytes, expected);
        assert_eq!(input.input.pages, 3);
        assert_eq!(input.input.rss_payloads, 1);
        assert!(
            input
                .candidates
                .iter()
                .all(|candidate| candidate.evidence[0].rss_event.is_some())
        );
        let payload_path = root.path().join(format!(
            "{}/events/{}.json",
            lk_core::rss_original::RSS_ORIGINAL_STORE,
            selected.revision
        ));
        std::fs::write(payload_path, "corrupt").unwrap();
        assert!(measure_inputs(root.path(), &input.candidates, u64::MAX).is_err());
        assert!(!root.path().join(STATE).exists());
    }

    #[test]
    fn receipts_skip_unchanged_inputs_and_track_evidence_and_related_scope() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let source = "---\ntype: document\ntitle: Source\n---\n## Summary\nA uses B under condition X.\n\n## Related Concepts\n- [A](../concepts/a.md)\n";
        let concept = "---\ntype: concept\ntitle: A\nupdated: 2026-01-01\nsource_count: 1\n---\n## Synthesis\nA uses B under X.\n\n## Sources\n- [Source](../documents/source.md)\n\n## Related\n- [B](b.md) — used by A under X.\n";
        let related =
            "---\ntype: concept\ntitle: B\n---\n## Synthesis\nB supplies an implementation.\n";
        write(root.path(), "wiki/documents/source.md", source);
        write(root.path(), "wiki/concepts/a.md", concept);
        write(root.path(), "wiki/concepts/b.md", related);
        let first = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        assert_eq!(first.total, 2);
        assert_eq!(first.unreviewed, 2);
        assert_eq!(first.candidates.len(), 1);
        assert_eq!(first.candidates[0].evidence.len(), 1);
        assert_eq!(first.candidates[0].related_pages.len(), 1);
        assert!(!root.path().join(STATE).exists());
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![assessment(&first.candidates[0])],
            },
            262_144,
        )
        .unwrap();
        let next = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        assert_eq!(next.reviewed_current, 1);
        assert_eq!(next.candidates[0].path, "wiki/concepts/b.md");
        write(
            root.path(),
            "wiki/concepts/a.md",
            &concept
                .replace("2026-01-01", "2026-10-19")
                .replace("source_count: 1", "source_count: 2"),
        );
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1, 262_144)
                .unwrap()
                .reviewed_current,
            1
        );
        for (path, changed, original) in [
            (
                "wiki/documents/source.md",
                source.replace("condition X", "condition Y"),
                source,
            ),
            (
                "wiki/concepts/b.md",
                related.replace("an implementation", "a different implementation"),
                related,
            ),
        ] {
            write(root.path(), path, &changed);
            let stale = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
            assert_eq!(stale.changed, 1);
            assert_eq!(stale.candidates[0].reason, "changed");
            assert_eq!(stale.candidates[0].path, "wiki/concepts/a.md");
            let before = std::fs::read(root.path().join(STATE)).unwrap();
            assert!(
                record(
                    &session,
                    &dirs,
                    &scan(root.path()),
                    AuditResults {
                        assessments: vec![assessment(&first.candidates[0])]
                    },
                    262_144,
                )
                .is_err()
            );
            assert_eq!(std::fs::read(root.path().join(STATE)).unwrap(), before);
            write(root.path(), path, original);
        }
    }

    #[test]
    fn pending_work_is_fair_under_continuous_changes_and_new_prefixes() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let a = "wiki/concepts/a.md";
        let z = "wiki/concepts/z.md";
        let concept = "---\ntype: concept\ntitle: Topic\n---\n## Synthesis\nScoped claim.\n";
        write(root.path(), a, concept);
        write(root.path(), z, concept);
        let session = MutationSession::acquire(root.path()).unwrap();
        let complete = |candidate: &AuditCandidate| {
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(candidate)],
                },
                262_144,
            )
            .unwrap();
        };
        let first = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        complete(&first.candidates[0]);
        write(root.path(), a, &concept.replace("Scoped", "Changed"));
        write(root.path(), "wiki/concepts/aa.md", concept);
        let changed = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        assert_eq!(changed.candidates[0].path, a);
        complete(&changed.candidates[0]);
        write(root.path(), a, &concept.replace("Scoped", "Changed again"));
        write(root.path(), "wiki/concepts/ab.md", concept);
        let waiting = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        assert_eq!(waiting.candidates[0].path, z);
        complete(&waiting.candidates[0]);
        let changed = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        assert_eq!(changed.candidates[0].path, a);
        complete(&changed.candidates[0]);
        write(root.path(), "wiki/concepts/aaa.md", concept);
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1, 262_144)
                .unwrap()
                .candidates[0]
                .path,
            "wiki/concepts/aa.md"
        );
    }

    #[test]
    fn follow_up_requires_a_current_scoped_durable_review() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/a.md";
        let body = "---\ntype: concept\ntitle: A\n---\n## Synthesis\nAn unsupported definition.\n";
        write(root.path(), path, body);
        let input = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        let mut result = assessment(&input.candidates[0]);
        result.disposition = AuditDisposition::FollowUp;
        let session = MutationSession::acquire(root.path()).unwrap();
        let copy = |value: &ConceptAssessment| {
            serde_json::from_value::<ConceptAssessment>(serde_json::to_value(value).unwrap())
                .unwrap()
        };
        assert!(
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![copy(&result)]
                },
                262_144,
            )
            .is_err()
        );
        assert!(!root.path().join(STATE).exists());
        let revision = source_revision(&lk_vault::parse_page(body).unwrap());
        let review = "wiki/explorations/gap.md";
        let content = format!(
            "---\ntype: exploration\ntitle: Gap\nreview: {}\n---\n## Question\nWhat supports this definition?\n",
            json!({"status":"open","subjects":[path],"evidence":[{"path":path,"revision":revision}],"resolution":null})
        );
        write(root.path(), review, &content);
        result.reviews = vec![review.into()];
        write(
            root.path(),
            review,
            &content.replace(
                &format!("\"subjects\":[\"{path}\"]"),
                "\"subjects\":[\"wiki/concepts/b.md\"]",
            ),
        );
        assert!(
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![copy(&result)]
                },
                262_144,
            )
            .is_err()
        );
        assert!(!root.path().join(STATE).exists());
        write(root.path(), review, &content);
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![result],
            },
            262_144,
        )
        .unwrap();
        let current = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        assert_eq!(current.reviewed_current, 1);
        assert_eq!(current.follow_up_current, 1);
        assert!(current.candidates.is_empty());
        std::fs::remove_file(root.path().join(review)).unwrap();
        let missing = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        assert_eq!(missing.reviewed_current, 0);
        assert_eq!(missing.changed, 1);
        assert_eq!(missing.candidates[0].path, path);
        write(
            root.path(),
            review,
            &content.replace(&revision, &"0".repeat(32)),
        );
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1, 262_144)
                .unwrap()
                .changed,
            1
        );
        write(root.path(), review, &content);
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1, 262_144)
                .unwrap()
                .reviewed_current,
            1
        );
        std::fs::write(root.path().join(STATE), "{invalid").unwrap();
        assert!(plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).is_err());
    }
}
