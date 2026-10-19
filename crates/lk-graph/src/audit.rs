//! Incremental semantic-review receipts. They track work, never establish factual truth.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

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
) -> Result<AuditPlan, GraphError> {
    plan_selected(root, dirs, pages, limit, &[])
}

/// Re-read exact batch members after repairs without expanding semantic work scope.
pub fn plan_selected(
    root: &Path,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
    limit: usize,
    selection: &[String],
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
    Ok(AuditPlan {
        candidates,
        total,
        reviewed_current,
        follow_up_current,
        unreviewed: unreviewed_count,
        changed: changed_count,
        limit,
    })
}

/// Record a completed read with current concept/evidence/related-page guards.
/// Findings remain in authored exploration reviews, not in this operational cache.
pub fn record(
    session: &MutationSession,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
    results: AuditResults,
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
        if candidate.file_revision != assessment.file_revision
            || candidate.input_revision != assessment.input_revision
        {
            return Err(error(format!(
                "audit inputs changed: {}; reread before recording",
                assessment.path
            )));
        }
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
        let first = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
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
        )
        .unwrap();
        let next = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
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
            plan(root.path(), &dirs, &scan(root.path()), 1)
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
            let stale = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
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
                    }
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
            )
            .unwrap();
        };
        let first = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
        complete(&first.candidates[0]);
        write(root.path(), a, &concept.replace("Scoped", "Changed"));
        write(root.path(), "wiki/concepts/aa.md", concept);
        let changed = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
        assert_eq!(changed.candidates[0].path, a);
        complete(&changed.candidates[0]);
        write(root.path(), a, &concept.replace("Scoped", "Changed again"));
        write(root.path(), "wiki/concepts/ab.md", concept);
        let waiting = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
        assert_eq!(waiting.candidates[0].path, z);
        complete(&waiting.candidates[0]);
        let changed = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
        assert_eq!(changed.candidates[0].path, a);
        complete(&changed.candidates[0]);
        write(root.path(), "wiki/concepts/aaa.md", concept);
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1)
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
        let input = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
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
                }
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
                }
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
        )
        .unwrap();
        let current = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
        assert_eq!(current.reviewed_current, 1);
        assert_eq!(current.follow_up_current, 1);
        assert!(current.candidates.is_empty());
        std::fs::remove_file(root.path().join(review)).unwrap();
        let missing = plan(root.path(), &dirs, &scan(root.path()), 1).unwrap();
        assert_eq!(missing.reviewed_current, 0);
        assert_eq!(missing.changed, 1);
        assert_eq!(missing.candidates[0].path, path);
        write(
            root.path(),
            review,
            &content.replace(&revision, &"0".repeat(32)),
        );
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1)
                .unwrap()
                .changed,
            1
        );
        write(root.path(), review, &content);
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1)
                .unwrap()
                .reviewed_current,
            1
        );
        std::fs::write(root.path().join(STATE), "{invalid").unwrap();
        assert!(plan(root.path(), &dirs, &scan(root.path()), 1).is_err());
    }
}
