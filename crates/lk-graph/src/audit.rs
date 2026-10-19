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
const CONCEPT_POLICY: &str = "concept-audit-v2";
const DOCUMENT_POLICY: &str = "document-audit-v1";

fn error(message: impl std::fmt::Display) -> GraphError {
    GraphError::Io(message.to_string())
}

#[derive(Clone, Copy, Deserialize, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuditDisposition {
    Reviewed,
    FollowUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditSubjectKind {
    Concept,
    Document,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditAssessment {
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
    pub assessments: Vec<AuditAssessment>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipts {
    entries: BTreeMap<String, AuditAssessment>,
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
    pub kind: AuditSubjectKind,
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
    let candidates = inputs(root, dirs, pages, &receipts)?
        .into_iter()
        .map(|candidate| (candidate.path.clone(), candidate))
        .collect::<BTreeMap<_, _>>();
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
                if candidate.kind != original.kind {
                    return Err(error(format!("audit subject kind changed: {path}")));
                }
                let receipt = receipts
                    .entries
                    .get(path)
                    .filter(|receipt| receipt_is_current(receipt, candidate))
                    .ok_or_else(|| {
                        error(format!("audit subject lacks a current assessment: {path}"))
                    })?;
                verified.push(AuditCompletion {
                    kind: candidate.kind,
                    requested_path: original.path.clone(),
                    assessed_path: path.into(),
                    disposition: receipt.disposition,
                });
                assessed.insert(candidate.path.clone(), candidate.clone());
                break;
            }
            if original.kind != AuditSubjectKind::Concept {
                return Err(error(format!(
                    "audit document is no longer a current document: {path}"
                )));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditReviewState {
    Missing,
    Invalid,
    NeedsReassessment,
    Open,
    Resolved,
}

impl AuditReviewState {
    fn is_current(self) -> bool {
        matches!(self, Self::Open | Self::Resolved)
    }
}

/// An exact readable page; authored identity excludes operational bookkeeping.
#[derive(Clone, Serialize)]
pub struct AuditPageInput {
    pub path: String,
    pub file_revision: String,
    pub source_revision: Option<String>,
}

impl AuditPageInput {
    fn authored_revision(&self) -> &str {
        self.source_revision
            .as_deref()
            .unwrap_or(&self.file_revision)
    }
}

#[derive(Clone, Serialize)]
pub struct AuditReviewEvidence {
    pub recorded: Citation,
    pub current: Option<AuditPageInput>,
    /// Verified prior snapshot when current evidence differs or is missing.
    pub archived: Option<AuditPageInput>,
}

#[derive(Clone, Serialize)]
pub struct AuditReviewDependency {
    pub path: String,
    pub file_revision: Option<String>,
    pub source_revision: Option<String>,
    pub state: AuditReviewState,
    pub errors: Vec<String>,
    pub evidence: Vec<AuditReviewEvidence>,
}

impl AuditReviewDependency {
    fn document(&self) -> Option<AuditPageInput> {
        Some(AuditPageInput {
            path: self.path.clone(),
            file_revision: self.file_revision.clone()?,
            source_revision: self.source_revision.clone(),
        })
    }

    fn identity(&self) -> serde_json::Value {
        let identity =
            |page: &AuditPageInput| serde_json::json!([page.path, page.authored_revision()]);
        serde_json::json!({
            "path": self.path,
            "revision": self.source_revision.as_ref().or(self.file_revision.as_ref()),
            "state": self.state,
            "evidence": self.evidence.iter().map(|evidence| serde_json::json!({
                "recorded": evidence.recorded,
                "current": evidence.current.as_ref().map(identity),
                "archived": evidence.archived.as_ref().map(identity),
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Serialize)]
pub struct AuditCandidate {
    pub kind: AuditSubjectKind,
    pub path: String,
    pub title: String,
    pub file_revision: String,
    pub input_revision: String,
    pub knowledge_status: KnowledgeStatus,
    pub evidence: Vec<Citation>,
    pub related_pages: Vec<Citation>,
    pub reviews: Vec<AuditReviewDependency>,
    pub reason: &'static str,
}

#[derive(Serialize)]
pub struct AuditPlan {
    pub candidates: Vec<AuditCandidate>,
    /// Individually oversized inputs encountered while selecting this automatic batch.
    /// At most 100 details, in selection order; exact selections do not defer inputs.
    pub blocked: Vec<AuditBlockedInput>,
    /// All blockers encountered before filling the batch, not a corpus-wide inventory.
    pub blocked_count: usize,
    pub total: usize,
    pub reviewed_current: usize,
    pub follow_up_current: usize,
    pub unreviewed: usize,
    pub changed: usize,
    pub counts_by_kind: BTreeMap<AuditSubjectKind, AuditCounts>,
    pub limit: usize,
    pub input: AuditInputBound,
}

#[derive(Default, Serialize)]
pub struct AuditCounts {
    pub total: usize,
    pub reviewed_current: usize,
    pub follow_up_current: usize,
    pub unreviewed: usize,
    pub changed: usize,
}

#[derive(Serialize)]
pub struct AuditBlockedInput {
    pub kind: AuditSubjectKind,
    pub path: String,
    pub file_revision: String,
    pub input_revision: String,
    pub input: AuditInputBound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditInputStatus {
    Ready,
    Oversized,
}

/// Complete distinct source footprint, not a bound on model tokens, calls or cost.
/// Pages include selected subjects, direct evidence, directly related context and scoped
/// review documents with their current and available verified historical evidence;
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

#[derive(Clone, Default)]
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

    fn include_candidate(
        &mut self,
        root: &Path,
        candidate: &AuditCandidate,
    ) -> Result<(), GraphError> {
        self.page(root, &candidate.path, Some(&candidate.file_revision), None)?;
        for citation in candidate.evidence.iter().chain(&candidate.related_pages) {
            if citation.rss_event.is_some() {
                self.page(root, &citation.path, None, None)?;
                self.rss_payload(root, &candidate.path, citation)?;
            } else {
                self.page(root, &citation.path, None, Some(&citation.revision))?;
            }
        }
        for review in &candidate.reviews {
            if let Some(document) = review.document() {
                self.page(
                    root,
                    &document.path,
                    Some(&document.file_revision),
                    document.source_revision.as_deref(),
                )?;
            } else if review.state == AuditReviewState::Invalid && exact_page_address(&review.path)
            {
                // Unreadable selected documents cannot masquerade as a zero-byte input.
                load_page_input(root, &review.path)?;
            }
            for evidence in &review.evidence {
                if evidence.current.is_none()
                    && exact_page_address(&evidence.recorded.path)
                    && load_page_input(root, &evidence.recorded.path)?.is_some()
                {
                    return Err(error(format!(
                        "audit review evidence appeared during input measurement: {}",
                        evidence.recorded.path
                    )));
                }
            }
            for page in review
                .evidence
                .iter()
                .flat_map(|evidence| evidence.current.iter().chain(&evidence.archived))
            {
                self.page(
                    root,
                    &page.path,
                    Some(&page.file_revision),
                    page.source_revision.as_deref(),
                )?;
            }
        }
        Ok(())
    }

    fn bound(&self, max_input_bytes: u64) -> Result<AuditInputBound, GraphError> {
        if max_input_bytes == 0 {
            return Err(error("audit max_input_bytes must be positive"));
        }
        let bytes = self
            .pages
            .values()
            .map(|(_, bytes)| *bytes)
            .chain(self.rss_payloads.values().copied())
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
            pages: self.pages.len(),
            rss_payloads: self.rss_payloads.len(),
        })
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
        resources.include_candidate(root, candidate)?;
    }
    resources.bound(max_input_bytes)
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

fn exact_page_address(path: &str) -> bool {
    lk_core::link::resolve_dest(Path::new(""), path).is_some_and(|resolved| {
        resolved.to_string_lossy() == path && resolved.extension().is_some_and(|ext| ext == "md")
    })
}

fn page_input(path: &str, raw: &str) -> AuditPageInput {
    AuditPageInput {
        path: path.into(),
        file_revision: file_revision(raw),
        source_revision: lk_vault::parse_page(raw)
            .ok()
            .map(|page| source_revision(&page)),
    }
}

fn load_page_input(
    root: &Path,
    path: &str,
) -> Result<Option<(String, AuditPageInput)>, GraphError> {
    let raw =
        match std::fs::read_to_string(lk_vault::page_path(root, Path::new(path)).map_err(error)?) {
            Ok(raw) => raw,
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(failure) => return Err(error(failure)),
        };
    let input = page_input(path, &raw);
    Ok(Some((raw, input)))
}

/// Retain explicit subjects even when another review metadata field is invalid.
/// Unreadable prose and malformed subjects cannot establish a semantic association.
fn review_subjects(
    root: &Path,
    reviews: &[lk_vault::ReviewReport],
) -> Result<BTreeMap<String, Vec<String>>, GraphError> {
    #[derive(Deserialize)]
    struct Subjects {
        #[serde(default)]
        subjects: Vec<String>,
    }
    let mut subjects = BTreeMap::new();
    for review in reviews {
        let scoped = if let Some(metadata) = &review.review {
            metadata.subjects.clone()
        } else {
            let raw = std::fs::read_to_string(
                lk_vault::page_path(root, Path::new(&review.path)).map_err(error)?,
            );
            raw.ok()
                .and_then(|raw| lk_vault::parse_page(&raw).ok())
                .and_then(|page| page.frontmatter.get("review").cloned())
                .and_then(|value| serde_json::from_value::<Subjects>(value).ok())
                .map(|scope| scope.subjects)
                .unwrap_or_default()
        };
        subjects.insert(review.path.clone(), scoped);
    }
    Ok(subjects)
}

fn scoped_reviews(
    root: &Path,
    subject: &Path,
    page: &lk_vault::VaultPage,
    reviews: &[lk_vault::ReviewReport],
    subjects: &BTreeMap<String, Vec<String>>,
    receipts: &Receipts,
) -> Result<Vec<AuditReviewDependency>, GraphError> {
    let subject = subject.to_string_lossy();
    let mut paths = subjects
        .iter()
        .filter(|(_, subjects)| subjects.iter().any(|declared| declared == subject.as_ref()))
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    // Preserve dependencies recorded by earlier assessments, including now missing reviews.
    if let Some(receipt) = receipts.entries.get(subject.as_ref()) {
        paths.extend(receipt.reviews.iter().cloned());
    }
    let mut result = Vec::new();
    if let Some(pointer) = page.frontmatter.get("knowledge_review") {
        if let Some(path) = pointer.as_str().filter(|path| exact_page_address(path)) {
            paths.insert(path.into());
        } else {
            result.push(AuditReviewDependency {
                path: pointer.as_str().unwrap_or("").into(),
                file_revision: None,
                source_revision: None,
                state: AuditReviewState::Invalid,
                errors: vec![
                    "knowledge_review must be an exact vault-relative Markdown address".into(),
                ],
                evidence: Vec::new(),
            });
        }
    }
    for path in paths {
        let report = reviews.iter().find(|review| review.path == path);
        result.push(review_dependency(
            root,
            &subject,
            &path,
            report,
            subjects.get(&path),
        )?);
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn review_dependency(
    root: &Path,
    subject: &str,
    path: &str,
    report: Option<&lk_vault::ReviewReport>,
    subjects: Option<&Vec<String>>,
) -> Result<AuditReviewDependency, GraphError> {
    let mut result = AuditReviewDependency {
        path: path.into(),
        file_revision: None,
        source_revision: None,
        state: AuditReviewState::Invalid,
        errors: Vec::new(),
        evidence: Vec::new(),
    };
    if !exact_page_address(path) {
        result
            .errors
            .push("review dependency is not an exact Markdown address".into());
        return Ok(result);
    }
    let (raw, document) = match load_page_input(root, path) {
        Ok(Some(input)) => input,
        Ok(None) => {
            result.state = AuditReviewState::Missing;
            result.errors.push("scoped review is missing".into());
            return Ok(result);
        }
        Err(failure) => {
            result.errors.push(failure.to_string());
            return Ok(result);
        }
    };
    result.file_revision = Some(document.file_revision);
    result.source_revision = document.source_revision;
    let Some(report) = report else {
        result
            .errors
            .push("scoped path is not a knowledge review".into());
        return Ok(result);
    };
    result.errors = report.errors.clone();
    result.state = match report.effective_state {
        lk_vault::ReviewEffectiveState::Open => AuditReviewState::Open,
        lk_vault::ReviewEffectiveState::Resolved => AuditReviewState::Resolved,
        lk_vault::ReviewEffectiveState::NeedsReassessment => AuditReviewState::NeedsReassessment,
        lk_vault::ReviewEffectiveState::Invalid => AuditReviewState::Invalid,
    };
    if !subjects.is_some_and(|subjects| subjects.iter().any(|declared| declared == subject)) {
        result.state = AuditReviewState::Invalid;
        result
            .errors
            .push("scoped review does not declare this audit subject".into());
    }
    let Some(metadata) = &report.review else {
        return Ok(result);
    };
    let current_metadata = lk_vault::parse_page(&raw)
        .ok()
        .and_then(|page| page.frontmatter.get("review").cloned())
        .and_then(|value| serde_json::from_value::<lk_vault::KnowledgeReview>(value).ok());
    if current_metadata
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(error)?
        != Some(serde_json::to_value(metadata).map_err(error)?)
    {
        return Err(error(format!(
            "review changed during audit planning: {path}"
        )));
    }
    for citation in &metadata.evidence {
        let current = if exact_page_address(&citation.path) {
            match load_page_input(root, &citation.path) {
                Ok(input) => input.map(|(_, input)| input),
                Err(failure) => {
                    result.state = AuditReviewState::Invalid;
                    result.errors.push(failure.to_string());
                    None
                }
            }
        } else {
            None
        };
        let current_supplies_recorded = current
            .as_ref()
            .is_some_and(|page| page.source_revision.as_deref() == Some(&citation.revision));
        let archived = if current_supplies_recorded {
            None
        } else {
            match lk_vault::load_archived_evidence(root, &citation.revision) {
                Ok(Some(page)) => {
                    let address =
                        lk_vault::evidence_archive_path(&citation.revision).map_err(error)?;
                    Some(page_input(
                        &address.to_string_lossy(),
                        page.original
                            .as_deref()
                            .expect("parsed archive retains exact bytes"),
                    ))
                }
                Ok(None) => None,
                Err(failure) => {
                    result.state = AuditReviewState::Invalid;
                    result.errors.push(failure.to_string());
                    None
                }
            }
        };
        if report.effective_state != lk_vault::ReviewEffectiveState::Invalid {
            let changed = current
                .as_ref()
                .is_some_and(|page| page.source_revision.as_deref() != Some(&citation.revision));
            if changed != report.changed_evidence.contains(&citation.path)
                || current.is_none() != report.missing_evidence.contains(&citation.path)
                || (!current_supplies_recorded
                    && archived.is_some() != report.archived_evidence.contains(&citation.path))
            {
                return Err(error(format!(
                    "review evidence changed during audit planning: {}",
                    citation.path
                )));
            }
        }
        result.evidence.push(AuditReviewEvidence {
            recorded: citation.clone(),
            current,
            archived,
        });
    }
    result.evidence.sort_by(|a, b| a.recorded.cmp(&b.recorded));
    Ok(result)
}

fn inputs(
    root: &Path,
    dirs: &VaultDirs,
    pages: &[ScannedPage],
    receipts: &Receipts,
) -> Result<Vec<AuditCandidate>, GraphError> {
    let reviews = lk_vault::list_reviews(root, dirs).map_err(error)?;
    let subjects = review_subjects(root, &reviews)?;
    let evidence = crate::backlinks::concept_evidence(pages, root, dirs)?;
    let documents = lk_core::vault_path::documents_dir(dirs);
    let kinds = pages
        .iter()
        .filter_map(|page| {
            let kind = if evidence.contains_key(&page.path) {
                AuditSubjectKind::Concept
            } else if page.format.as_deref() == Some("document")
                && page
                    .path
                    .extension()
                    .is_some_and(|extension| extension == "md")
                && crate::scan::under(&lk_core::vault_path::path_slug(&page.path), &documents)
            {
                AuditSubjectKind::Document
            } else {
                return None;
            };
            Some((page.path.clone(), kind))
        })
        .collect::<BTreeMap<_, _>>();
    let by_id: BTreeMap<_, _> = pages.iter().map(|page| (page.id.as_str(), page)).collect();
    let existence = crate::scan::VaultExistence::build_at(root, pages, dirs)?;
    let mut result = Vec::new();
    for page in pages.iter().filter(|page| kinds.contains_key(&page.path)) {
        let kind = kinds[&page.path];
        let raw = std::fs::read_to_string(lk_vault::page_path(root, &page.path).map_err(error)?)
            .map_err(error)?;
        let parsed = lk_vault::parse_page(&raw).map_err(error)?;
        let revision = source_revision(&parsed);
        if page.revision.as_deref() != Some(&revision) {
            return Err(error("subject changed during audit planning"));
        }
        let mut related_pages = Vec::new();
        for related in page
            .outgoing
            .iter()
            .filter_map(|link| existence.reached(link))
            .filter_map(|id| by_id.get(id))
        {
            let is_context = match kind {
                AuditSubjectKind::Concept => evidence.contains_key(&related.path),
                AuditSubjectKind::Document => kinds.contains_key(&related.path),
            };
            if related.path != page.path && is_context {
                related_pages.push(Citation::at_revision(
                    &related.path,
                    related
                        .revision
                        .clone()
                        .ok_or_else(|| error("unreadable related subject context"))?,
                ));
            }
        }
        related_pages.sort();
        related_pages.dedup();
        // A document is the subject being read, not evidence for itself. Its outgoing
        // links supply context only; concepts retain the shared backlink authority.
        let citations = match kind {
            AuditSubjectKind::Concept => evidence[&page.path].clone(),
            AuditSubjectKind::Document => Vec::new(),
        };
        let scoped_reviews =
            scoped_reviews(root, &page.path, &parsed, &reviews, &subjects, receipts)?;
        let review_identity = scoped_reviews
            .iter()
            .map(AuditReviewDependency::identity)
            .collect::<Vec<_>>();
        let input_revision = file_revision(
            &serde_json::to_string(&(
                match kind {
                    AuditSubjectKind::Concept => CONCEPT_POLICY,
                    AuditSubjectKind::Document => DOCUMENT_POLICY,
                },
                &revision,
                &citations,
                &related_pages,
                review_identity,
            ))
            .map_err(error)?,
        );
        result.push(AuditCandidate {
            kind,
            path: page.path.to_string_lossy().into_owned(),
            title: page.title.clone(),
            file_revision: file_revision(&raw),
            input_revision,
            knowledge_status: parsed.frontmatter.knowledge_status().map_err(error)?,
            evidence: citations,
            related_pages,
            reviews: scoped_reviews,
            reason: "unreviewed",
        });
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn receipt_is_current(receipt: &AuditAssessment, candidate: &AuditCandidate) -> bool {
    receipt.input_revision == candidate.input_revision
        && candidate
            .reviews
            .iter()
            .all(|review| review.state.is_current())
        && (receipt.disposition != AuditDisposition::FollowUp || !candidate.reviews.is_empty())
        && receipt.reviews.iter().all(|path| {
            candidate
                .reviews
                .iter()
                .any(|review| review.path == *path && review.state.is_current())
        })
}

fn pending(
    candidates: Vec<AuditCandidate>,
    receipts: &Receipts,
) -> (Vec<AuditCandidate>, BTreeMap<AuditSubjectKind, AuditCounts>) {
    let mut remaining = BTreeMap::new();
    let mut counts = BTreeMap::from([
        (AuditSubjectKind::Concept, AuditCounts::default()),
        (AuditSubjectKind::Document, AuditCounts::default()),
    ]);
    for mut candidate in candidates {
        let count = counts
            .get_mut(&candidate.kind)
            .expect("enumerated subject kind");
        count.total += 1;
        match receipts.entries.get(&candidate.path) {
            Some(receipt) if receipt_is_current(receipt, &candidate) => {
                count.reviewed_current += 1;
                if receipt.disposition == AuditDisposition::FollowUp {
                    count.follow_up_current += 1;
                }
            }
            previous => {
                if previous.is_some() {
                    candidate.reason = "changed";
                    count.changed += 1;
                } else {
                    count.unreviewed += 1;
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
    (work, counts)
}

/// Rotate changed and never-reviewed work, retaining first-discovery order in each class.
/// Report oversized inputs as pending blockers while selecting later fitting work.
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
    let candidates = inputs(root, dirs, pages, &receipts)?;
    let total = candidates.len();
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
            .ok_or_else(|| error("audit selection is not an exact concept or document address"))?
            .clone();
        candidate.reason = match receipts.entries.get(path) {
            Some(receipt) if receipt_is_current(receipt, &candidate) => "current",
            Some(_) => "changed",
            None => "unreviewed",
        };
        selected.push(candidate);
    }
    let (pending, counts_by_kind) = pending(candidates, &receipts);
    let reviewed_current = counts_by_kind
        .values()
        .map(|counts| counts.reviewed_current)
        .sum();
    let follow_up_current = counts_by_kind
        .values()
        .map(|counts| counts.follow_up_current)
        .sum();
    let (mut changed, mut unreviewed): (VecDeque<_>, VecDeque<_>) = pending
        .into_iter()
        .partition(|candidate| candidate.reason == "changed");
    let changed_count = changed.len();
    let unreviewed_count = unreviewed.len();
    let mut candidates = selected;
    let mut resources = InputResources::default();
    let mut input = if selection.is_empty() {
        resources.bound(max_input_bytes)?
    } else {
        measure_inputs(root, &candidates, max_input_bytes)?
    };
    let mut blocked = Vec::new();
    let mut blocked_count = 0;
    let mut next_class = receipts.next_class;
    while selection.is_empty() && candidates.len() < limit {
        let next = match next_class {
            AuditClass::Changed => changed.pop_front().or_else(|| unreviewed.pop_front()),
            AuditClass::Unreviewed => unreviewed.pop_front().or_else(|| changed.pop_front()),
        };
        let Some(candidate) = next else { break };
        let mut prospective = resources.clone();
        prospective.include_candidate(root, &candidate)?;
        let combined = prospective.bound(max_input_bytes)?;
        if combined.status == AuditInputStatus::Ready {
            next_class = if candidate.reason == "changed" {
                AuditClass::Unreviewed
            } else {
                AuditClass::Changed
            };
            candidates.push(candidate);
            resources = prospective;
            input = combined;
            continue;
        }
        let individual = if candidates.is_empty() {
            combined
        } else {
            measure_inputs(root, std::iter::once(&candidate), max_input_bytes)?
        };
        if individual.status == AuditInputStatus::Ready {
            // This complete input fits a fresh batch. Preserve its position instead
            // of treating remaining batch capacity as an individual blocker.
            break;
        }
        blocked_count += 1;
        if blocked.len() < 100 {
            blocked.push(AuditBlockedInput {
                kind: candidate.kind,
                path: candidate.path,
                file_revision: candidate.file_revision,
                input_revision: candidate.input_revision,
                input: individual,
            });
        }
    }
    Ok(AuditPlan {
        candidates,
        blocked,
        blocked_count,
        total,
        reviewed_current,
        follow_up_current,
        unreviewed: unreviewed_count,
        changed: changed_count,
        counts_by_kind,
        limit,
        input,
    })
}

fn guard_page_input(
    plan: &mut MutationPlan,
    root: &Path,
    input: &AuditPageInput,
) -> Result<(), GraphError> {
    let Some((raw, current)) = load_page_input(root, &input.path)? else {
        return Err(error(format!(
            "audit read dependency disappeared: {}",
            input.path
        )));
    };
    if current.file_revision != input.file_revision {
        return Err(error(format!(
            "audit read dependency changed: {}",
            input.path
        )));
    }
    let canonical_root = root.canonicalize().map_err(error)?;
    let canonical = lk_vault::page_path(root, Path::new(&input.path))
        .map_err(error)?
        .canonicalize()
        .map_err(error)?;
    let direct = canonical.strip_prefix(&canonical_root).map_err(error)?;
    plan.require_unchanged(direct, raw);
    Ok(())
}

fn guard_review_dependency(
    plan: &mut MutationPlan,
    root: &Path,
    review: &AuditReviewDependency,
) -> Result<(), GraphError> {
    if let Some(document) = review.document() {
        guard_page_input(plan, root, &document)?;
    }
    for evidence in &review.evidence {
        if let Some(current) = &evidence.current {
            guard_page_input(plan, root, current)?;
        } else {
            plan.require_absent(&evidence.recorded.path);
        }
        if let Some(archived) = &evidence.archived {
            guard_page_input(plan, root, archived)?;
        }
    }
    Ok(())
}

/// Record a completed read with subject, evidence, context-page and scoped-review guards.
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
    let candidates = inputs(root, dirs, pages, &receipts)?;
    let (work, _) = pending(candidates.clone(), &receipts);
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
                .ok_or_else(|| error("audit target is not an exact concept or document address"))?;
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
    for mut assessment in results.assessments {
        if !recorded.insert(assessment.path.clone()) || assessment.reason.trim().is_empty() {
            return Err(error(
                "audit assessments require distinct paths and a substantive reason",
            ));
        }
        let candidate = candidates
            .get(&assessment.path)
            .ok_or_else(|| error("audit target is not an exact concept or document address"))?;
        if let Some(review) = candidate
            .reviews
            .iter()
            .find(|review| !review.state.is_current())
        {
            return Err(error(format!(
                "audit scoped review is not current: {} ({:?}); reassess or repair before recording",
                review.path, review.state
            )));
        }
        if assessment.disposition == AuditDisposition::FollowUp && candidate.reviews.is_empty() {
            return Err(error("audit follow-up requires a durable scoped review"));
        }
        for path in &assessment.reviews {
            if !candidate
                .reviews
                .iter()
                .any(|review| review.path == *path && review.state.is_current())
            {
                return Err(error("audit review must currently assess this subject"));
            }
        }
        assessment.reviews = candidate
            .reviews
            .iter()
            .map(|review| review.path.clone())
            .collect();
        for review in &candidate.reviews {
            guard_review_dependency(&mut plan, root, review)?;
        }
        guard_page_input(
            &mut plan,
            root,
            &AuditPageInput {
                path: candidate.path.clone(),
                file_revision: candidate.file_revision.clone(),
                source_revision: None,
            },
        )?;
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
                guard_page_input(&mut plan, root, &page_input(&citation.path, &raw))?;
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
    fn assessment(candidate: &AuditCandidate) -> AuditAssessment {
        AuditAssessment {path:candidate.path.clone(),file_revision:candidate.file_revision.clone(),input_revision:candidate.input_revision.clone(),disposition:AuditDisposition::Reviewed,reason:"Read the complete source and concept; the scoped claim and relationship are supported.".into(),reviews:vec![]}
    }

    fn review_body(subject: &str, evidence: &str, revision: &str) -> String {
        format!(
            "---\ntype: exploration\nupdated: 2026-01-01\nreview: {}\n---\n## Judgment\nThe claim needs a documented follow-up.\n",
            json!({"status":"open","subjects":[subject],"evidence":[{"path":evidence,"revision":revision}],"resolution":null})
        )
    }

    #[test]
    #[cfg(unix)]
    fn symlink_subjects_record_public_addresses_and_reject_retargeted_reads() {
        for (kind, directory, format) in [
            (AuditSubjectKind::Concept, "wiki/concepts", "concept"),
            (AuditSubjectKind::Document, "wiki/documents", "document"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let dirs = VaultDirs::default();
            let alias = format!("{directory}/a-alias.md");
            let owner = format!("{directory}/z-owner.md");
            let other = format!("{directory}/other.md");
            let body = format!("---\ntype: {format}\n---\nThe captured authored subject.\n");
            write(root.path(), &owner, &body);
            write(root.path(), &other, &body.replace("captured", "different"));
            std::os::unix::fs::symlink("z-owner.md", root.path().join(&alias)).unwrap();
            let selected = plan_selected(
                root.path(),
                &dirs,
                &scan(root.path()),
                1,
                std::slice::from_ref(&alias),
                u64::MAX,
            )
            .unwrap();
            let candidate = &selected.candidates[0];
            assert_eq!(candidate.kind, kind);
            assert_eq!(candidate.path, alias);
            let session = MutationSession::acquire(root.path()).unwrap();
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(candidate)],
                },
                u64::MAX,
            )
            .unwrap();
            assert!(
                std::fs::symlink_metadata(root.path().join(&alias))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            let before = std::fs::read(root.path().join(STATE)).unwrap();
            let prepared = plan_selected(
                root.path(),
                &dirs,
                &scan(root.path()),
                1,
                std::slice::from_ref(&alias),
                u64::MAX,
            )
            .unwrap();
            std::fs::remove_file(root.path().join(&alias)).unwrap();
            std::os::unix::fs::symlink("other.md", root.path().join(&alias)).unwrap();
            let rejected = record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(&prepared.candidates[0])],
                },
                u64::MAX,
            )
            .unwrap_err();
            assert!(rejected.to_string().contains("audit inputs changed"));
            assert_eq!(std::fs::read(root.path().join(STATE)).unwrap(), before);
        }
    }

    #[test]
    #[cfg(unix)]
    fn symlink_ordinary_evidence_and_context_use_guarded_owner_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let subject = "wiki/concepts/a.md";
        let context = "wiki/concepts/b-alias.md";
        let evidence = "wiki/documents/evidence-alias.md";
        write(
            root.path(),
            subject,
            "---\ntype: concept\n---\nThe scoped claim. [Context](b-alias.md).\n",
        );
        write(
            root.path(),
            "wiki/concepts/b-owner.md",
            "---\ntype: concept\n---\nContext for the scoped claim.\n",
        );
        write(
            root.path(),
            ".private/evidence.md",
            "---\ntype: document\n---\nThe [claim](../concepts/a.md) has supporting evidence.\n",
        );
        std::fs::create_dir_all(root.path().join("wiki/documents")).unwrap();
        std::os::unix::fs::symlink("b-owner.md", root.path().join(context)).unwrap();
        std::os::unix::fs::symlink("../../.private/evidence.md", root.path().join(evidence))
            .unwrap();
        let selected = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[subject.into()],
            u64::MAX,
        )
        .unwrap();
        let candidate = &selected.candidates[0];
        assert_eq!(candidate.evidence[0].path, evidence);
        assert_eq!(candidate.related_pages[0].path, context);
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![assessment(candidate)],
            },
            u64::MAX,
        )
        .unwrap();
        for alias in [context, evidence] {
            assert!(
                std::fs::symlink_metadata(root.path().join(alias))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        let owner = root.path().join(".private/evidence.md");
        let old = std::fs::read_to_string(&owner).unwrap();
        std::fs::write(&owner, old.replace("supporting", "changed")).unwrap();
        let before = std::fs::read(root.path().join(STATE)).unwrap();
        assert!(
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(candidate)]
                },
                u64::MAX
            )
            .is_err()
        );
        assert_eq!(std::fs::read(root.path().join(STATE)).unwrap(), before);
    }

    #[test]
    fn uncited_documents_have_explicit_subject_kind_and_only_direct_context() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/documents/team/source.md";
        let body = "---\ntype: document\ntitle: Source\n---\nAn authored source. [Topic](../../concepts/a.md), [context](../other.md), [self](source.md).\n";
        write(root.path(), path, body);
        write(
            root.path(),
            "wiki/concepts/a.md",
            "---\ntype: concept\n---\nA scoped claim.\n",
        );
        write(
            root.path(),
            "wiki/documents/other.md",
            "---\ntype: document\n---\nContext links [onward](third.md).\n",
        );
        write(
            root.path(),
            "wiki/documents/third.md",
            "---\ntype: document\n---\nTransitive context.\n",
        );
        write(
            root.path(),
            "wiki/documents/not-a-document.md",
            "---\ntype: exploration\n---\nNot a source document subject.\n",
        );
        let selected = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        let candidate = &selected.candidates[0];
        assert_eq!(candidate.kind, AuditSubjectKind::Document);
        assert_eq!(candidate.path, path);
        assert!(
            candidate.evidence.is_empty(),
            "a document is not evidence for itself or for the content it links"
        );
        assert_eq!(
            candidate
                .related_pages
                .iter()
                .map(|page| page.path.as_str())
                .collect::<Vec<_>>(),
            vec!["wiki/concepts/a.md", "wiki/documents/other.md"]
        );
        assert_eq!(selected.input.pages, 3);
        assert_eq!(selected.total, 4);
        assert_eq!(selected.counts_by_kind[&AuditSubjectKind::Concept].total, 1);
        assert_eq!(
            selected.counts_by_kind[&AuditSubjectKind::Document].unreviewed,
            3
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![assessment(candidate)],
            },
            u64::MAX,
        )
        .unwrap();
        let current = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(current.candidates[0].reason, "current");
        assert_eq!(
            current.counts_by_kind[&AuditSubjectKind::Document].reviewed_current,
            1
        );
        for changed in [path, "wiki/documents/other.md"] {
            let before = std::fs::read_to_string(root.path().join(changed)).unwrap();
            write(
                root.path(),
                changed,
                &format!("{before}Changed authored context.\n"),
            );
            let stale = plan_selected(
                root.path(),
                &dirs,
                &scan(root.path()),
                1,
                &[path.into()],
                u64::MAX,
            )
            .unwrap();
            assert_eq!(stale.candidates[0].reason, "changed");
            assert_ne!(stale.candidates[0].input_revision, candidate.input_revision);
            assert!(
                record(
                    &session,
                    &dirs,
                    &scan(root.path()),
                    AuditResults {
                        assessments: vec![assessment(candidate)]
                    },
                    u64::MAX
                )
                .is_err()
            );
            write(root.path(), changed, &before);
        }
        let old = std::fs::read_to_string(root.path().join("wiki/documents/third.md")).unwrap();
        write(
            root.path(),
            "wiki/documents/third.md",
            &format!("{old}Unselected change.\n"),
        );
        let unchanged = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(unchanged.candidates[0].reason, "current");
        assert_eq!(
            unchanged.candidates[0].input_revision,
            candidate.input_revision
        );
    }

    #[test]
    fn document_scoped_reviews_guard_explicit_evidence_and_cannot_be_omitted() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/documents/report.md";
        let source = "wiki/documents/evidence.md";
        let review = "wiki/explorations/report-review.md";
        let body = "---\ntype: document\n---\nService X production policy effective 2026-01-01: publication retries are advisory.\n";
        let original = "---\ntype: document\n---\nService X production policy effective 2026-01-01: publication retries are mandatory.\n";
        write(root.path(), path, body);
        write(root.path(), source, original);
        let revision = source_revision(&lk_vault::parse_page(original).unwrap());
        let metadata = json!({
            "status": "open", "subjects": [path, source], "resolution": null,
            "evidence": [
                {"path": path, "revision": source_revision(&lk_vault::parse_page(body).unwrap())},
                {"path": source, "revision": revision},
            ],
        });
        write(
            root.path(),
            review,
            &format!(
                "---\ntype: exploration\nreview: {metadata}\n---\nThe documents give incompatible requirements for the same service, environment and effective date. The authorized policy remains unverified.\n"
            ),
        );
        let ready = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        let candidate = &ready.candidates[0];
        assert!(candidate.evidence.is_empty());
        assert!(candidate.related_pages.is_empty());
        assert_eq!(candidate.reviews.len(), 1);
        assert_eq!(candidate.reviews[0].evidence.len(), 2);
        assert_eq!(ready.counts_by_kind[&AuditSubjectKind::Concept].total, 0);
        assert_eq!(ready.counts_by_kind[&AuditSubjectKind::Document].total, 2);
        assert_eq!(ready.input.pages, 3);
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut completed = assessment(candidate);
        completed.disposition = AuditDisposition::FollowUp;
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![completed],
            },
            u64::MAX,
        )
        .unwrap();
        let current = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(current.follow_up_current, 1);
        assert_eq!(
            current.counts_by_kind[&AuditSubjectKind::Document].follow_up_current,
            1
        );
        lk_vault::archive_evidence(root.path(), &[source.into()]).unwrap();
        write(
            root.path(),
            source,
            &format!("{original}Changed supporting observation.\n"),
        );
        let stale = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        let candidate = &stale.candidates[0];
        assert_eq!(candidate.reason, "changed");
        assert_eq!(
            candidate.reviews[0].state,
            AuditReviewState::NeedsReassessment
        );
        assert!(candidate.reviews[0].evidence[0].archived.is_some());
        assert_eq!(stale.input.pages, 4);
        let before = std::fs::read(root.path().join(STATE)).unwrap();
        assert!(
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(candidate)]
                },
                u64::MAX
            )
            .unwrap_err()
            .to_string()
            .contains("scoped review is not current")
        );
        assert_eq!(std::fs::read(root.path().join(STATE)).unwrap(), before);
    }

    #[test]
    fn document_completion_requires_the_current_document_at_its_exact_address() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/documents/report.md";
        let body = "---\ntype: document\n---\nAn uncited source document.\n";
        write(root.path(), path, body);
        write(
            root.path(),
            "wiki/concepts/survivor.md",
            "---\ntype: concept\n---\nAnother subject.\n",
        );
        let selected = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: selected.candidates.iter().map(assessment).collect(),
            },
            u64::MAX,
        )
        .unwrap();
        let workflow = lk_core::execution::WorkflowId::from_bytes([3; 16]);
        let completion = verify_batch(
            root.path(),
            &dirs,
            &scan(root.path()),
            &selected.candidates,
            &workflow,
            u64::MAX,
        )
        .unwrap();
        assert_eq!(completion[0].kind, AuditSubjectKind::Document);
        assert_eq!(completion[0].requested_path, path);
        assert_eq!(completion[0].assessed_path, path);
        for replacement in [Some(body.replace("type: document", "type: concept")), None] {
            match replacement {
                Some(replacement) => write(root.path(), path, &replacement),
                None => std::fs::remove_file(root.path().join(path)).unwrap(),
            }
            let failure = verify_batch(
                root.path(),
                &dirs,
                &scan(root.path()),
                &selected.candidates,
                &workflow,
                u64::MAX,
            )
            .err()
            .unwrap();
            assert!(failure.to_string().contains("no longer a current document"));
        }
    }

    #[test]
    fn adding_document_subjects_preserves_existing_concept_input_identities() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/a.md";
        let body = "---\ntype: concept\n---\nA scoped claim.\n";
        write(root.path(), path, body);
        write(
            root.path(),
            "wiki/documents/source.md",
            "---\ntype: document\n---\n[Claim](../concepts/a.md).\n",
        );
        let selected = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        let candidate = &selected.candidates[0];
        let previous_identity = file_revision(
            &serde_json::to_string(&(
                "concept-audit-v2",
                source_revision(&lk_vault::parse_page(body).unwrap()),
                &candidate.evidence,
                &candidate.related_pages,
                Vec::<serde_json::Value>::new(),
            ))
            .unwrap(),
        );
        assert_eq!(candidate.input_revision, previous_identity);
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![assessment(candidate)],
            },
            u64::MAX,
        )
        .unwrap();
        let current = plan(root.path(), &dirs, &scan(root.path()), 2, u64::MAX).unwrap();
        assert_eq!(current.reviewed_current, 1);
        assert_eq!(
            current.counts_by_kind[&AuditSubjectKind::Concept].reviewed_current,
            1
        );
        assert_eq!(current.candidates.len(), 1);
        assert_eq!(current.candidates[0].kind, AuditSubjectKind::Document);
        assert_eq!(current.candidates[0].reason, "unreviewed");
    }

    #[test]
    fn document_bounds_and_exact_mixed_batches_keep_complete_scope() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let document = "wiki/documents/source.md";
        let body = "---\ntype: document\n---\nComplete authored source.\n";
        write(root.path(), document, body);
        let blocked = plan(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            body.len() as u64 - 1,
        )
        .unwrap();
        assert!(blocked.candidates.is_empty());
        assert_eq!(blocked.blocked_count, 1);
        assert_eq!(blocked.blocked[0].kind, AuditSubjectKind::Document);
        assert_eq!(blocked.blocked[0].input.bytes, body.len() as u64);
        assert!(!root.path().join(STATE).exists());
        let concept = "wiki/concepts/a.md";
        let concept_body = "---\ntype: concept\n---\nA scoped claim.\n";
        write(root.path(), concept, concept_body);
        let selection = [document.into(), concept.into()];
        let selected = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            2,
            &selection,
            body.len() as u64,
        )
        .unwrap();
        assert_eq!(
            selected
                .candidates
                .iter()
                .map(|candidate| candidate.path.clone())
                .collect::<Vec<_>>(),
            selection
        );
        assert_eq!(selected.input.status, AuditInputStatus::Oversized);
        assert_eq!(
            selected.input.bytes,
            (body.len() + concept_body.len()) as u64
        );
        assert!(selected.blocked.is_empty());
    }

    #[test]
    fn scoped_review_drift_cannot_be_omitted_and_reads_current_plus_prior_evidence() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/a.md";
        let review = "wiki/explorations/finding.md";
        let source = "wiki/documents/source.md";
        let concept = "---\ntype: concept\n---\nA claim.\n";
        let original = "---\ntype: document\nupdated: 2026-01-01\n---\nPrior evidence.\n";
        write(root.path(), path, concept);
        write(root.path(), source, original);
        let revision = source_revision(&lk_vault::parse_page(original).unwrap());
        let content = review_body(path, source, &revision);
        write(root.path(), review, &content);
        let ready = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        let dependency = &ready.candidates[0].reviews[0];
        assert_eq!(dependency.state, AuditReviewState::Open);
        assert!(dependency.evidence[0].archived.is_none());
        assert_eq!(ready.input.pages, 3);
        assert_eq!(
            ready.input.bytes,
            (concept.len() + content.len() + original.len()) as u64
        );
        let mut completed = assessment(&ready.candidates[0]);
        completed.disposition = AuditDisposition::FollowUp;
        assert!(
            completed.reviews.is_empty(),
            "caller need not repeat automatically scoped paths"
        );
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![completed],
            },
            u64::MAX,
        )
        .unwrap();
        let (_, receipts) = load(root.path()).unwrap();
        assert_eq!(receipts.entries[path].reviews, [review]);
        let current = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        assert_eq!(current.follow_up_current, 1);
        lk_vault::archive_evidence(root.path(), &[source.into()]).unwrap();
        let archived = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        assert_eq!(
            archived.reviewed_current, 1,
            "identical archive creation adds history, not a new semantic input"
        );
        let exact = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(exact.input.bytes, ready.input.bytes);
        assert!(
            exact.candidates[0].reviews[0].evidence[0]
                .archived
                .is_none()
        );
        // Raw bookkeeping changes update exact read guards without reopening semantic work.
        write(
            root.path(),
            source,
            &original.replace("2026-01-01", "2026-10-19"),
        );
        write(
            root.path(),
            review,
            &content.replace("2026-01-01", "2026-10-19"),
        );
        assert_eq!(
            plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX)
                .unwrap()
                .reviewed_current,
            1
        );
        let changed = original.replace("Prior evidence.", "Revised evidence with another meaning.");
        write(root.path(), source, &changed);
        let stale = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        assert_eq!(stale.changed, 1);
        assert_eq!(stale.reviewed_current, 0);
        let evidence = &stale.candidates[0].reviews[0].evidence[0];
        assert_eq!(
            stale.candidates[0].reviews[0].state,
            AuditReviewState::NeedsReassessment
        );
        assert_eq!(evidence.recorded.revision, revision);
        assert_ne!(
            evidence.current.as_ref().unwrap().source_revision,
            Some(revision.clone())
        );
        assert_eq!(
            evidence.archived.as_ref().unwrap().source_revision,
            Some(revision)
        );
        let before = std::fs::read(root.path().join(STATE)).unwrap();
        let rejected = record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![assessment(&stale.candidates[0])],
            },
            u64::MAX,
        )
        .unwrap_err();
        assert!(
            rejected
                .to_string()
                .contains("scoped review is not current")
        );
        assert_eq!(std::fs::read(root.path().join(STATE)).unwrap(), before);
        let workflow = lk_core::execution::WorkflowId::from_bytes([5; 16]);
        assert!(
            verify_batch(
                root.path(),
                &dirs,
                &scan(root.path()),
                &ready.candidates,
                &workflow,
                u64::MAX
            )
            .is_err()
        );
        let oversized = plan(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            stale.input.bytes - 1,
        )
        .unwrap();
        assert_eq!(oversized.candidates.len(), 1);
        assert_eq!(oversized.candidates[0].kind, AuditSubjectKind::Document);
        assert_eq!(oversized.candidates[0].path, source);
        assert_eq!(oversized.blocked_count, 1);
        assert_eq!(oversized.blocked[0].kind, AuditSubjectKind::Concept);
        assert_eq!(
            oversized.blocked[0].input.status,
            AuditInputStatus::Oversized
        );
        assert_eq!(oversized.blocked[0].input.bytes, stale.input.bytes);
        assert_eq!(
            stale.input.bytes,
            (concept.len() + content.len() + changed.len() + original.len()) as u64
        );
    }

    #[test]
    fn scoped_missing_invalid_and_unrelated_reviews_have_distinct_effects() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/a.md";
        let review = "wiki/explorations/finding.md";
        let concept = "---\ntype: concept\n---\nA claim.\n";
        write(root.path(), path, concept);
        let source = "wiki/documents/source.md";
        write(root.path(), source, "Source evidence.\n");
        let revision = source_revision(&lk_vault::parse_page("Source evidence.\n").unwrap());
        // An invalid review for another explicit subject is not a global audit blocker.
        write(
            root.path(),
            review,
            &review_body(source, source, &revision)
                .replace("\"evidence\":[", "\"invalid_field\":true,\"evidence\":["),
        );
        let unrelated = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        assert!(unrelated.candidates[0].reviews.is_empty());
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![assessment(&unrelated.candidates[0])],
            },
            u64::MAX,
        )
        .unwrap();
        // Scope-only typed subject parsing retains the association despite invalid metadata.
        let invalid = review_body(path, source, &revision)
            .replace("\"evidence\":[", "\"invalid_field\":true,\"evidence\":[");
        write(root.path(), review, &invalid);
        let selected = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(
            selected.candidates[0].reviews[0].state,
            AuditReviewState::Invalid
        );
        assert!(
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(&selected.candidates[0])]
                },
                u64::MAX
            )
            .is_err()
        );
        std::fs::remove_file(root.path().join(review)).unwrap();
        write(
            root.path(),
            path,
            &concept.replace(
                "type: concept",
                &format!("type: concept\nknowledge_review: {review}"),
            ),
        );
        let selected = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            &[path.into()],
            u64::MAX,
        )
        .unwrap();
        assert_eq!(
            selected.candidates[0].reviews[0].state,
            AuditReviewState::Missing
        );
        assert!(
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(&selected.candidates[0])]
                },
                u64::MAX
            )
            .is_err()
        );
    }

    #[test]
    fn unreadable_evidence_in_an_unselected_review_is_not_a_global_blocker() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for path in ["wiki/concepts/a.md", "wiki/concepts/b.md"] {
            write(root.path(), path, "---\ntype: concept\n---\nA claim.\n");
        }
        std::fs::create_dir_all(root.path().join("notes")).unwrap();
        std::fs::write(root.path().join("notes/unreadable.md"), [0xff]).unwrap();
        write(
            root.path(),
            "wiki/explorations/finding.md",
            &review_body("wiki/concepts/b.md", "notes/unreadable.md", &"0".repeat(32)),
        );
        let pages = scan(root.path());
        let selected = plan_selected(
            root.path(),
            &dirs,
            &pages,
            1,
            &["wiki/concepts/a.md".into()],
            u64::MAX,
        )
        .unwrap();
        assert!(selected.candidates[0].reviews.is_empty());
        let session = MutationSession::acquire(root.path()).unwrap();
        record(
            &session,
            &dirs,
            &pages,
            AuditResults {
                assessments: vec![assessment(&selected.candidates[0])],
            },
            u64::MAX,
        )
        .unwrap();
        // The invalid dependency remains explicit for B; its unreadable bytes cannot be
        // represented as a ready complete footprint for an actual selected read.
        let (_, receipts) = load(root.path()).unwrap();
        let candidates = inputs(root.path(), &dirs, &pages, &receipts).unwrap();
        assert_eq!(candidates[1].reviews[0].state, AuditReviewState::Invalid);
        assert!(
            plan_selected(
                root.path(),
                &dirs,
                &pages,
                1,
                &["wiki/concepts/b.md".into()],
                u64::MAX
            )
            .is_err()
        );
    }

    #[test]
    fn archived_only_review_evidence_stays_valid_and_deleted_review_cannot_be_erased() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/a.md";
        let review = "wiki/explorations/finding.md";
        let source = "wiki/documents/source.md";
        write(root.path(), path, "---\ntype: concept\n---\nA claim.\n");
        write(root.path(), source, "Historical evidence.\n");
        let archived = lk_vault::archive_evidence(root.path(), &[source.into()]).unwrap();
        write(
            root.path(),
            review,
            &review_body(path, source, &archived.citations[0].revision),
        );
        std::fs::remove_file(root.path().join(source)).unwrap();
        let ready = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        let dep = &ready.candidates[0].reviews[0];
        assert_eq!(dep.state, AuditReviewState::Open);
        assert!(dep.evidence[0].current.is_none());
        assert!(dep.evidence[0].archived.is_some());
        assert_eq!(ready.input.pages, 3);
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut guarded = MutationPlan::default();
        guard_review_dependency(&mut guarded, root.path(), dep).unwrap();
        write(root.path(), source, "Newly appeared evidence.\n");
        assert!(session.apply(guarded).is_err());
        assert!(measure_inputs(root.path(), &ready.candidates, u64::MAX).is_err());
        assert!(!root.path().join(STATE).exists());
        std::fs::remove_file(root.path().join(source)).unwrap();
        record(
            &session,
            &dirs,
            &scan(root.path()),
            AuditResults {
                assessments: vec![assessment(&ready.candidates[0])],
            },
            u64::MAX,
        )
        .unwrap();
        std::fs::remove_file(root.path().join(review)).unwrap();
        let missing = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        assert_eq!(
            missing.candidates[0].reviews[0].state,
            AuditReviewState::Missing
        );
        assert!(
            record(
                &session,
                &dirs,
                &scan(root.path()),
                AuditResults {
                    assessments: vec![assessment(&missing.candidates[0])]
                },
                u64::MAX
            )
            .is_err()
        );
    }

    #[test]
    fn scoped_review_read_guards_refuse_intervening_document_or_archive_edits() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let path = "wiki/concepts/a.md";
        let review = "wiki/explorations/finding.md";
        let source = "wiki/documents/source.md";
        write(root.path(), path, "---\ntype: concept\n---\nA claim.\n");
        write(root.path(), source, "Evidence.\n");
        let archive = lk_vault::archive_evidence(root.path(), &[source.into()]).unwrap();
        write(
            root.path(),
            review,
            &review_body(path, source, &archive.citations[0].revision),
        );
        write(root.path(), source, "Revised evidence.\n");
        let ready = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
        let dep = &ready.candidates[0].reviews[0];
        let session = MutationSession::acquire(root.path()).unwrap();
        for input in [
            dep.document().unwrap(),
            dep.evidence[0].current.clone().unwrap(),
            dep.evidence[0].archived.clone().unwrap(),
        ] {
            let mut guarded = MutationPlan::default();
            guard_page_input(&mut guarded, root.path(), &input).unwrap();
            let before = std::fs::read_to_string(root.path().join(&input.path)).unwrap();
            write(
                root.path(),
                &input.path,
                &format!("{before}External edit.\n"),
            );
            assert!(session.apply(guarded).is_err());
            assert!(measure_inputs(root.path(), &ready.candidates, u64::MAX).is_err());
            write(root.path(), &input.path, &before);
        }
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
        let paths = ready
            .candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect::<Vec<_>>();
        let oversized = plan_selected(root.path(), &dirs, &pages, 2, &paths, expected - 1).unwrap();
        assert_eq!(oversized.input.status, AuditInputStatus::Oversized);
        assert_eq!(oversized.blocked_count, 0);
        assert!(oversized.blocked.is_empty());
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
    fn oversized_pending_inputs_do_not_block_later_work_or_gain_receipts() {
        for previously_reviewed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let dirs = VaultDirs::default();
            let large = "wiki/concepts/a.md";
            let small = "---\ntype: concept\ntitle: Topic\n---\nA scoped claim.\n";
            write(root.path(), large, small);
            let session = MutationSession::acquire(root.path()).unwrap();
            if previously_reviewed {
                let initial = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
                record(
                    &session,
                    &dirs,
                    &scan(root.path()),
                    AuditResults {
                        assessments: vec![assessment(&initial.candidates[0])],
                    },
                    u64::MAX,
                )
                .unwrap();
            }
            let large_body = format!("{small}{}", "Large evidence context.\n".repeat(100));
            write(root.path(), large, &large_body);
            for path in ["wiki/concepts/b.md", "wiki/concepts/c.md"] {
                write(root.path(), path, small);
            }
            let cap = small.len() as u64;
            for expected in ["wiki/concepts/b.md", "wiki/concepts/c.md"] {
                let before = std::fs::read(root.path().join(STATE)).ok();
                let selected = plan(root.path(), &dirs, &scan(root.path()), 1, cap).unwrap();
                assert_eq!(selected.candidates.len(), 1);
                assert_eq!(selected.candidates[0].path, expected);
                assert_eq!(selected.input.status, AuditInputStatus::Ready);
                assert_eq!(selected.blocked_count, 1);
                assert_eq!(selected.blocked[0].path, large);
                assert_eq!(selected.blocked[0].input.bytes, large_body.len() as u64);
                let repeated = plan(root.path(), &dirs, &scan(root.path()), 1, cap).unwrap();
                assert_eq!(repeated.candidates[0].path, expected);
                assert_eq!(
                    repeated.blocked[0].input_revision,
                    selected.blocked[0].input_revision
                );
                assert_eq!(std::fs::read(root.path().join(STATE)).ok(), before);
                record(
                    &session,
                    &dirs,
                    &scan(root.path()),
                    AuditResults {
                        assessments: vec![assessment(&selected.candidates[0])],
                    },
                    cap,
                )
                .unwrap();
            }
            let before = std::fs::read(root.path().join(STATE)).unwrap();
            let blocked = plan(root.path(), &dirs, &scan(root.path()), 1, cap).unwrap();
            assert!(blocked.candidates.is_empty());
            assert_eq!(blocked.blocked_count, 1);
            assert_eq!(blocked.reviewed_current, 2);
            assert_eq!(blocked.changed, usize::from(previously_reviewed));
            assert_eq!(blocked.unreviewed, usize::from(!previously_reviewed));
            assert_eq!(blocked.input.bytes, 0);
            assert_eq!(std::fs::read(root.path().join(STATE)).unwrap(), before);

            let resumed = plan(root.path(), &dirs, &scan(root.path()), 1, u64::MAX).unwrap();
            assert_eq!(resumed.candidates[0].path, large);
            assert_eq!(resumed.blocked_count, 0);
            assert_eq!(
                resumed.candidates[0].input_revision, blocked.blocked[0].input_revision,
                "resource policy changes cannot rebind semantic identities"
            );
        }
    }

    #[test]
    fn oversized_inputs_do_not_consume_the_preferred_class_turn() {
        for prefer_changed in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let dirs = VaultDirs::default();
            let blocked = "wiki/concepts/a.md";
            let fitting = "wiki/concepts/c.md";
            let other_first = "wiki/concepts/u1.md";
            let other_second = "wiki/concepts/u2.md";
            let original = "---\ntype: concept\n---\nA scoped claim.\n";
            let changed = original.replace("scoped", "reread");
            let reviewed = if prefer_changed {
                [blocked, fitting]
            } else {
                [other_first, other_second]
            };
            for path in reviewed {
                write(root.path(), path, original);
            }
            let session = MutationSession::acquire(root.path()).unwrap();
            let complete = |candidate: &AuditCandidate| {
                record(
                    &session,
                    &dirs,
                    &scan(root.path()),
                    AuditResults {
                        assessments: vec![assessment(candidate)],
                    },
                    u64::MAX,
                )
                .unwrap();
            };
            let initial = plan(root.path(), &dirs, &scan(root.path()), 2, u64::MAX).unwrap();
            for candidate in &initial.candidates {
                complete(candidate);
            }
            if !prefer_changed {
                // Recording an already assessed member makes unreviewed work next.
                complete(&initial.candidates[0]);
            }
            for path in reviewed {
                write(root.path(), path, &changed);
            }
            let new = if prefer_changed {
                [other_first, other_second]
            } else {
                [blocked, fitting]
            };
            for path in new {
                write(root.path(), path, original);
            }
            write(
                root.path(),
                blocked,
                &format!("{changed}{}", "Context.\n".repeat(100)),
            );
            let cap = changed.len() as u64;
            let first = plan(root.path(), &dirs, &scan(root.path()), 1, cap).unwrap();
            assert_eq!(
                first.candidates[0].path, fitting,
                "an oversized member must not pass its class turn to the other class (prefer_changed={prefer_changed})"
            );
            assert_eq!(first.blocked_count, 1);
            assert_eq!(first.blocked[0].path, blocked);
            complete(&first.candidates[0]);
            let opposite = plan(root.path(), &dirs, &scan(root.path()), 1, cap).unwrap();
            assert_eq!(opposite.candidates[0].path, other_first);
            complete(&opposite.candidates[0]);
            let exhausted = plan(root.path(), &dirs, &scan(root.path()), 1, cap).unwrap();
            assert_eq!(exhausted.candidates[0].path, other_second);
            assert_eq!(exhausted.blocked[0].path, blocked);
        }
    }

    #[test]
    fn batch_capacity_defers_a_fitting_input_without_calling_it_oversized() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = "---\ntype: concept\ntitle: Topic\n---\nA scoped claim.\n";
        for path in ["wiki/concepts/a.md", "wiki/concepts/b.md"] {
            write(root.path(), path, concept);
        }
        let cap = concept.len() as u64;
        let selected = plan(root.path(), &dirs, &scan(root.path()), 2, cap).unwrap();
        assert_eq!(selected.candidates.len(), 1);
        assert_eq!(selected.candidates[0].path, "wiki/concepts/a.md");
        assert_eq!(selected.input.bytes, cap);
        assert_eq!(selected.input.pages, 1);
        assert_eq!(selected.blocked_count, 0);
        assert!(selected.blocked.is_empty());
        let exact = plan_selected(
            root.path(),
            &dirs,
            &scan(root.path()),
            2,
            &["wiki/concepts/b.md".into(), "wiki/concepts/a.md".into()],
            cap,
        )
        .unwrap();
        assert_eq!(exact.candidates.len(), 2);
        assert_eq!(exact.candidates[0].path, "wiki/concepts/b.md");
        assert_eq!(exact.input.bytes, cap * 2);
        assert_eq!(exact.input.status, AuditInputStatus::Oversized);
        assert_eq!(exact.blocked_count, 0);
        assert!(!root.path().join(STATE).exists());
    }

    #[test]
    fn oversized_details_are_bounded_and_selection_stops_when_full() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let concept = "---\ntype: concept\n---\nA scoped claim.\n";
        for index in 0..102 {
            write(
                root.path(),
                &format!("wiki/concepts/{index:03}.md"),
                concept,
            );
        }
        let blocked = plan(root.path(), &dirs, &scan(root.path()), 1, 1).unwrap();
        assert!(blocked.candidates.is_empty());
        assert_eq!(blocked.blocked_count, 102);
        assert_eq!(blocked.blocked.len(), 100);
        assert_eq!(blocked.blocked[0].path, "wiki/concepts/000.md");
        assert_eq!(blocked.blocked[99].path, "wiki/concepts/099.md");
        assert_eq!(blocked.unreviewed, 102);

        let small = "---\ntype: concept\n---\n";
        write(root.path(), "wiki/concepts/000.md", small);
        let selected = plan(
            root.path(),
            &dirs,
            &scan(root.path()),
            1,
            small.len() as u64,
        )
        .unwrap();
        assert_eq!(selected.candidates.len(), 1);
        assert_eq!(selected.candidates[0].path, "wiki/concepts/000.md");
        assert_eq!(selected.blocked_count, 0);
        assert!(!root.path().join(STATE).exists());
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
        let candidates =
            inputs(root.path(), &dirs, &scan(root.path()), &Receipts::default()).unwrap();
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

    #[test]
    fn incremental_measurement_refuses_exact_drift_in_a_shared_dependency() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let source_path = "wiki/documents/shared.md";
        let source = "---\ntype: document\nupdated: 2026-01-01\n---\nShared evidence.\n## Related Concepts\n- [A](../concepts/a.md)\n- [B](../concepts/b.md)\n";
        let concept = "---\ntype: concept\n---\nA scoped claim.\n";
        write(root.path(), source_path, source);
        for path in ["wiki/concepts/a.md", "wiki/concepts/b.md"] {
            write(root.path(), path, concept);
        }
        let ready = plan(root.path(), &dirs, &scan(root.path()), 2, u64::MAX).unwrap();
        let mut resources = InputResources::default();
        resources
            .include_candidate(root.path(), &ready.candidates[0])
            .unwrap();
        let changed = source.replace("2026-01-01", "2026-01-02");
        assert_eq!(
            source_revision(&lk_vault::parse_page(source).unwrap()),
            source_revision(&lk_vault::parse_page(&changed).unwrap()),
            "bookkeeping drift leaves the semantic source identity unchanged"
        );
        write(root.path(), source_path, &changed);
        let mut prospective = resources.clone();
        assert!(
            prospective
                .include_candidate(root.path(), &ready.candidates[1])
                .is_err()
        );
        assert_eq!(resources.bound(u64::MAX).unwrap().pages, 2);
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
        assert_eq!(first.total, 3);
        assert_eq!(first.unreviewed, 3);
        assert_eq!(
            first.counts_by_kind[&AuditSubjectKind::Concept].unreviewed,
            2
        );
        assert_eq!(
            first.counts_by_kind[&AuditSubjectKind::Document].unreviewed,
            1
        );
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
        let copy = |value: &AuditAssessment| {
            serde_json::from_value::<AuditAssessment>(serde_json::to_value(value).unwrap()).unwrap()
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
        let refreshed = plan(root.path(), &dirs, &scan(root.path()), 1, 262_144).unwrap();
        result.file_revision = refreshed.candidates[0].file_revision.clone();
        result.input_revision = refreshed.candidates[0].input_revision.clone();
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
