//! Durable assessments live in authored exploration pages, outside regenerated synthesis.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_core::concept::Citation;
use lk_core::config::VaultDirs;
use serde::{Deserialize, Serialize};

use crate::{VaultError, parse_page};

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewStatus {
    Open,
    Resolved,
}

#[derive(Debug, Serialize)]
pub struct KnowledgeReview {
    pub status: ReviewStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<String>,
    pub evidence: Vec<Citation>,
    pub resolution: Option<String>,
}

impl<'de> Deserialize<'de> for KnowledgeReview {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            status: ReviewStatus,
            #[serde(default)]
            subjects: Vec<String>,
            #[serde(deserialize_with = "deserialize_page_evidence")]
            evidence: Vec<Citation>,
            resolution: Option<String>,
        }
        let mapping = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let fields: Fields = serde_json::from_value(serde_json::Value::Object(mapping))
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            status: fields.status,
            subjects: fields.subjects,
            evidence: fields.evidence,
            resolution: fields.resolution,
        })
    }
}

fn deserialize_page_evidence<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Citation>, D::Error> {
    let citations = Vec::<Citation>::deserialize(deserializer)?;
    if citations
        .iter()
        .any(|citation| citation.rss_event.is_some())
    {
        return Err(serde::de::Error::custom(
            "review evidence requires authored-page citations; RSS event selectors are synthesis evidence",
        ));
    }
    Ok(citations)
}

impl KnowledgeReview {
    pub(crate) fn validation_errors(&self, path: &Path, page: &crate::VaultPage) -> Vec<String> {
        let mut errors = Vec::new();
        if page
            .frontmatter
            .get("type")
            .and_then(serde_json::Value::as_str)
            != Some("exploration")
            || page.frontmatter.get("llm_inputs").is_some()
        {
            errors.push("knowledge reviews require complete authored exploration pages".into());
        }
        let valid_address = |address: &str| {
            lk_core::link::resolve_dest(Path::new(""), address).is_some_and(|resolved| {
                resolved.to_string_lossy() == address
                    && resolved
                        .extension()
                        .is_some_and(|extension| extension == "md")
            })
        };
        let mut subjects = std::collections::BTreeSet::new();
        for subject in &self.subjects {
            if !valid_address(subject) || !subjects.insert(subject) {
                errors.push(format!("invalid or duplicate review subject: {subject}"));
            }
        }
        if self.evidence.is_empty() {
            errors.push("review requires evidence".into());
        }
        if self.status == ReviewStatus::Resolved
            && self
                .resolution
                .as_deref()
                .is_none_or(|resolution| resolution.trim().is_empty())
        {
            errors.push("resolved review requires a resolution".into());
        }
        let mut evidence = std::collections::BTreeSet::new();
        for citation in &self.evidence {
            if !valid_address(&citation.path) || !evidence.insert(&citation.path) {
                errors.push(format!(
                    "invalid or duplicate review evidence: {}",
                    citation.path
                ));
            }
            if Path::new(&citation.path) == path {
                errors.push("a review cannot serve as its own evidence".into());
            }
            if let Err(error) = evidence_archive_path(&citation.revision) {
                errors.push(error.to_string());
            }
        }
        errors
    }

    /// Refuse removing historical evidence unless its exact recorded revision is
    /// verified in the archive. A surviving concept cannot substitute for it.
    pub fn ensure_evidence_paths_preserved(
        root: &Path,
        page_paths: &[std::path::PathBuf],
        removed_paths: &[std::path::PathBuf],
    ) -> Result<(), VaultError> {
        for path in page_paths {
            // Destructive preflight needs proof of absence, not a guess from
            // raw text. An illegible page may carry historical review metadata.
            let raw = crate::page_path(root, path).and_then(std::fs::read_to_string).map_err(|error| {
                VaultError::Io(std::io::Error::new(error.kind(), format!(
                    "cannot establish review evidence safety for {}: {error}; repair the page before merging or renaming",
                    path.display()
                )))
            })?;
            let page = parse_page(&raw).map_err(|error| VaultError::Frontmatter(format!(
                "cannot establish review evidence safety for {}: {error}; repair the page before merging or renaming",
                path.display()
            )))?;
            let Some(value) = page.frontmatter.get("review") else {
                continue;
            };
            let review: Self = serde_json::from_value(value.clone())
                .map_err(|e| VaultError::Frontmatter(format!("{}: {e}", path.display())))?;
            for citation in review.evidence {
                let evidence = lk_core::link::resolve_dest(Path::new(""), &citation.path)
                    .ok_or_else(|| {
                        VaultError::Frontmatter(format!(
                            "{}: invalid evidence path {}",
                            path.display(),
                            citation.path
                        ))
                    })?;
                let mut removed = false;
                for path in removed_paths {
                    removed |=
                        path == &evidence || crate::same_existing_entry(root, path, &evidence)?;
                }
                if removed && load_archived_evidence(root, &citation.revision)?.is_none() {
                    return Err(VaultError::Frontmatter(format!(
                        "refusing to remove {}: review {} records it as historical evidence without a verified archive; archive its recorded revision before merging or renaming",
                        evidence.display(),
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Plan current subject/pointer migration separately from immutable historical citations.
/// Returned snapshots include the exact inputs needed to compose each caller's page edits.
pub fn plan_review_repointing(
    root: &Path,
    page_paths: &[PathBuf],
    redirects: &[(PathBuf, PathBuf)],
    plan: &mut crate::MutationPlan,
) -> Result<BTreeMap<PathBuf, (String, String)>, VaultError> {
    fn address(value: &str) -> Result<PathBuf, VaultError> {
        lk_core::link::resolve_dest(Path::new(""), value)
            .filter(|path| {
                path.to_string_lossy() == value
                    && path.extension().is_some_and(|extension| extension == "md")
            })
            .ok_or_else(|| {
                VaultError::Frontmatter(format!("invalid knowledge review address: {value}"))
            })
    }

    fn destination(
        resolver: &mut crate::ExistingPageResolver,
        path: &Path,
        redirects: &BTreeMap<PathBuf, PathBuf>,
    ) -> Result<Option<PathBuf>, VaultError> {
        Ok(resolver
            .resolve(path)?
            .and_then(|owner| redirects.get(&owner).cloned()))
    }

    fn set<T: Serialize>(raw: &str, key: &str, value: &T) -> Result<String, VaultError> {
        let value = serde_json::to_string(value)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        crate::set_frontmatter_field(raw, key, &value)
            .ok_or_else(|| VaultError::Frontmatter("review migration requires frontmatter".into()))
    }

    let mut destinations = BTreeMap::new();
    for (from, into) in redirects {
        if destinations
            .insert(from.clone(), into.clone())
            .is_some_and(|prior| prior != *into)
        {
            return Err(VaultError::Frontmatter(
                "review address has conflicting destinations".into(),
            ));
        }
    }
    let mut resolver = crate::ExistingPageResolver::new(root, destinations.keys().cloned())?;
    let mut reviews = BTreeMap::new();
    let mut pointer_subjects: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut updates = BTreeMap::new();
    for path in page_paths {
        let raw = std::fs::read_to_string(crate::page_path(root, path)?)?;
        let page = parse_page(&raw).map_err(VaultError::Frontmatter)?;
        if let Some(value) = page.frontmatter.get("review") {
            let review: KnowledgeReview = serde_json::from_value(value.clone())
                .map_err(|error| VaultError::Frontmatter(error.to_string()))?;
            reviews.insert(
                path.clone(),
                (lk_core::concept::file_revision(&raw), review),
            );
        }
        let Some(pointer) = page.frontmatter.get("knowledge_review") else {
            continue;
        };
        let moved_subject = destination(&mut resolver, path, &destinations)?;
        let review_path = pointer.as_str().and_then(|pointer| address(pointer).ok());
        let Some(review_path) = review_path else {
            if moved_subject.is_some() {
                return Err(VaultError::Frontmatter(
                    "moved subject requires an exact knowledge_review address".into(),
                ));
            }
            continue;
        };
        let moved_review = destination(&mut resolver, &review_path, &destinations)?;
        if moved_subject.is_some() {
            pointer_subjects
                .entry(review_path)
                .or_default()
                .insert(path.to_string_lossy().into_owned());
        }
        if moved_subject.is_some() || moved_review.is_some() {
            let after = match moved_review {
                Some(review) => set(&raw, "knowledge_review", &review.to_string_lossy())?,
                None => raw.clone(),
            };
            updates.insert(path.clone(), (raw, after));
        }
    }
    let mut review_resolver = crate::ExistingPageResolver::new(root, reviews.keys().cloned())?;
    for (pointer, subjects) in pointer_subjects {
        let owner = review_resolver.resolve(&pointer)?.ok_or_else(|| {
            VaultError::Frontmatter(format!(
                "moved subject's knowledge_review is not an available authored review: {}",
                pointer.display()
            ))
        })?;
        let declared = reviews[&owner]
            .1
            .subjects
            .iter()
            .map(|subject| crate::resolve_page_owner(root, &address(subject)?))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let requested = subjects
            .iter()
            .map(|subject| crate::resolve_page_owner(root, &address(subject)?))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if !requested.is_subset(&declared) {
            return Err(VaultError::Frontmatter(format!(
                "knowledge_review does not declare the moved subject: {}; explicitly reassess its scope before moving or merging the subject",
                owner.display()
            )));
        }
    }
    let mut archives = BTreeSet::new();
    for (path, (revision, mut review)) in reviews {
        let mut subjects = Vec::new();
        let mut unique = BTreeSet::new();
        let mut changed = false;
        for subject in &review.subjects {
            let target = match address(subject) {
                Ok(path) => destination(&mut resolver, &path, &destinations)?
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|| subject.clone()),
                Err(_) => subject.clone(),
            };
            changed |= target != *subject;
            if unique.insert(target.clone()) {
                subjects.push(target);
            }
        }
        if !changed {
            continue;
        }
        let raw = std::fs::read_to_string(crate::page_path(root, &path)?)?;
        if lk_core::concept::file_revision(&raw) != revision {
            return Err(VaultError::Frontmatter(format!(
                "review changed during address migration: {}",
                path.display()
            )));
        }
        let page = parse_page(&raw).map_err(VaultError::Frontmatter)?;
        let errors = review.validation_errors(&path, &page);
        if !errors.is_empty() {
            return Err(VaultError::Frontmatter(errors.join("; ")));
        }
        let authored_revision = lk_core::concept::source_revision(&page);
        let archive = evidence_archive_path(&authored_revision)?;
        if archives.insert(archive.clone()) {
            match load_archived_evidence(root, &authored_revision)? {
                Some(page) => plan.require_unchanged(
                    archive,
                    page.original.expect("parsed archive retains exact bytes"),
                ),
                None => plan.create(archive, raw.clone()),
            }
        }
        review.subjects = subjects;
        let current = updates
            .remove(&path)
            .map(|(_, after)| after)
            .unwrap_or_else(|| raw.clone());
        updates.insert(path, (raw, set(&current, "review", &review)?));
    }
    Ok(updates)
}

/// Current usability of an authored judgment; never rewrites its historical status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewEffectiveState {
    Open,
    Resolved,
    NeedsReassessment,
    Invalid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPreview {
    pub path: String,
    pub effective_state: ReviewEffectiveState,
    pub relation: String,
}

pub(crate) fn review_previews(
    root: &Path,
    dirs: &VaultDirs,
) -> Result<BTreeMap<String, Vec<ReviewPreview>>, VaultError> {
    let mut result: BTreeMap<String, Vec<ReviewPreview>> = BTreeMap::new();
    for report in list_reviews(root, dirs)? {
        let mut subjects = BTreeSet::new();
        let mut evidence = BTreeSet::new();
        for (address, subject) in report.subjects().iter().map(|path| (path, true)).chain(
            report
                .review
                .iter()
                .flat_map(|review| &review.evidence)
                .map(|citation| (&citation.path, false)),
        ) {
            let Some(path) = lk_core::link::resolve_dest(Path::new(""), address).filter(|path| {
                path.to_string_lossy() == *address
                    && path.extension().is_some_and(|extension| extension == "md")
            }) else {
                continue;
            };
            let owner = match crate::find_page_owner(root, &path) {
                Ok(owner) => owner,
                Err(_) if report.effective_state == ReviewEffectiveState::Invalid => continue,
                Err(error) => return Err(error),
            };
            let Some(owner) = owner else {
                continue;
            };
            let owner = owner.to_string_lossy().into_owned();
            if subject {
                subjects.insert(owner);
            } else {
                evidence.insert(owner);
            }
        }
        for path in subjects.union(&evidence) {
            result.entry(path.clone()).or_default().push(ReviewPreview {
                path: report.path.clone(),
                effective_state: report.effective_state,
                relation: if subjects.contains(path) {
                    "subject"
                } else {
                    "evidence"
                }
                .into(),
            });
        }
    }
    Ok(result)
}

impl ReviewEffectiveState {
    pub fn requires_attention(self) -> bool {
        matches!(self, Self::NeedsReassessment | Self::Invalid)
    }
}

#[derive(Default)]
struct ReviewScope {
    subjects: Vec<String>,
}

impl ReviewScope {
    fn from_metadata(value: &serde_json::Value) -> Self {
        let subjects = value
            .as_object()
            .and_then(|mapping| mapping.get("subjects"))
            .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
            .unwrap_or_default();
        Self { subjects }
    }
}

#[derive(Serialize)]
pub struct ReviewReport {
    pub path: String,
    pub review: Option<KnowledgeReview>,
    pub effective_state: ReviewEffectiveState,
    pub changed_evidence: Vec<String>,
    pub archived_evidence: Vec<String>,
    pub missing_evidence: Vec<String>,
    pub errors: Vec<String>,
    #[serde(skip)]
    scope: ReviewScope,
}

impl ReviewReport {
    /// Explicit subjects from the same parsed metadata as this report's diagnostics.
    /// Other invalid fields do not erase scope or establish a valid assessment.
    pub fn subjects(&self) -> &[String] {
        &self.scope.subjects
    }

    fn classify(&self) -> ReviewEffectiveState {
        let Some(review) = &self.review else {
            return ReviewEffectiveState::Invalid;
        };
        if !self.errors.is_empty() {
            ReviewEffectiveState::Invalid
        } else if !self.changed_evidence.is_empty() {
            ReviewEffectiveState::NeedsReassessment
        } else {
            match review.status {
                ReviewStatus::Open => ReviewEffectiveState::Open,
                ReviewStatus::Resolved => ReviewEffectiveState::Resolved,
            }
        }
    }
}

#[derive(Serialize)]
pub struct EvidenceReport {
    pub citations: Vec<Citation>,
    /// Exact file snapshots, including bookkeeping, for guarded publication.
    pub file_revisions: BTreeMap<String, String>,
    /// Equal recorded addresses, not a count of independent observations.
    pub origins: BTreeMap<String, Vec<String>>,
    pub unknown_origins: Vec<String>,
}

fn read_source_snapshot(root: &Path, path: &str) -> Result<(String, crate::VaultPage), VaultError> {
    let path = lk_core::link::resolve_dest(Path::new(""), path)
        .ok_or_else(|| VaultError::Frontmatter("invalid evidence path".into()))?;
    let text = std::fs::read_to_string(crate::page_path(root, &path)?)?;
    let page = parse_page(&text).map_err(|e| VaultError::Frontmatter(e.to_string()))?;
    Ok((text, page))
}

fn read_source(root: &Path, path: &str) -> Result<crate::VaultPage, VaultError> {
    read_source_snapshot(root, path).map(|(_, page)| page)
}

pub fn evidence_archive_path(revision: &str) -> Result<std::path::PathBuf, VaultError> {
    if revision.len() != 32
        || !revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(VaultError::Frontmatter(
            "invalid evidence revision: expected BLAKE3-128 lowercase hex".into(),
        ));
    }
    Ok(Path::new(".lorekeeper/evidence").join(format!("{revision}.md")))
}

/// Whether an exact vault address names an available, verified evidence archive.
pub fn is_archived_evidence(root: &Path, path: &Path) -> Result<bool, VaultError> {
    let Some(revision) = path.file_stem().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    if evidence_archive_path(revision).ok().as_deref() != Some(path) {
        return Ok(false);
    }
    Ok(load_archived_evidence(root, revision)?.is_some())
}

/// Retrieve a historical source only after verifying its authored revision.
/// Missing archives are distinct from corrupt or unreadable archives.
pub fn load_archived_evidence(
    root: &Path,
    revision: &str,
) -> Result<Option<crate::VaultPage>, VaultError> {
    let path = evidence_archive_path(revision)?;
    let raw = match std::fs::read_to_string(crate::page_path(root, &path)?) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let page = parse_page(&raw).map_err(|error| {
        VaultError::Frontmatter(format!(
            "corrupt evidence archive {}: {error}",
            path.display()
        ))
    })?;
    if lk_core::concept::source_revision(&page) != revision {
        return Err(VaultError::Frontmatter(format!(
            "corrupt evidence archive {}: authored revision mismatch",
            path.display()
        )));
    }
    Ok(Some(page))
}

fn evidence_report(snapshots: &[(String, String, crate::VaultPage)]) -> EvidenceReport {
    let mut report = EvidenceReport {
        citations: Vec::new(),
        file_revisions: BTreeMap::new(),
        origins: BTreeMap::new(),
        unknown_origins: Vec::new(),
    };
    for (path, raw, page) in snapshots {
        report
            .file_revisions
            .insert(path.clone(), lk_core::concept::file_revision(raw));
        report.citations.push(Citation::new(Path::new(path), page));
        if let Some(origin) = page
            .frontmatter
            .get("source_url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            report
                .origins
                .entry(origin.to_string())
                .or_default()
                .push(path.clone());
        } else {
            report.unknown_origins.push(path.clone());
        }
    }
    report
}

fn read_snapshots(
    root: &Path,
    paths: &[String],
) -> Result<Vec<(String, String, crate::VaultPage)>, VaultError> {
    paths
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|path| {
            let (raw, page) = read_source_snapshot(root, path)?;
            Ok((path.clone(), raw, page))
        })
        .collect()
}

/// Inspect exactly the requested current pages. Missing provenance stays unknown.
pub fn inspect_evidence(root: &Path, paths: &[String]) -> Result<EvidenceReport, VaultError> {
    Ok(evidence_report(&read_snapshots(root, paths)?))
}

/// Preserve the exact source bytes read, addressed by authored revision. Every input
/// and existing archive is validated before publishing any new snapshot.
pub fn archive_evidence(root: &Path, paths: &[String]) -> Result<EvidenceReport, VaultError> {
    let snapshots = read_snapshots(root, paths)?;
    let report = evidence_report(&snapshots);
    for citation in &report.citations {
        load_archived_evidence(root, &citation.revision)?;
    }
    for ((_, raw, _), citation) in snapshots.iter().zip(&report.citations) {
        if load_archived_evidence(root, &citation.revision)?.is_some() {
            continue;
        }
        publish_archive(root, &citation.revision, raw)?;
    }
    Ok(report)
}

/// Publish an imported document without discarding its previous authored evidence.
/// The archive is durable before the exact-byte guarded target edit is attempted.
pub fn publish_document(
    session: &crate::MutationSession,
    path: &Path,
    content: &str,
) -> Result<(), VaultError> {
    publish_document_expected(session, path, content, None)
}

/// Retain the derivation snapshot through evidence capture and publication.
pub fn publish_document_expected(
    session: &crate::MutationSession,
    path: &Path,
    content: &str,
    expected: Option<&Option<String>>,
) -> Result<(), VaultError> {
    publish_document_expected_with_plan(
        session,
        path,
        content,
        expected,
        crate::MutationPlan::default(),
    )
}

/// Extend an archive-before-edit publication with already validated immutable dependencies.
pub fn publish_document_expected_with_plan(
    session: &crate::MutationSession,
    path: &Path,
    content: &str,
    expected: Option<&Option<String>>,
    mut plan: crate::MutationPlan,
) -> Result<(), VaultError> {
    plan_document_publication(session, path, content, expected, &mut plan)?;
    session.apply(plan)
}

/// Append an archive-before-edit publication to a caller-owned guarded mutation.
pub fn plan_document_publication(
    session: &crate::MutationSession,
    path: &Path,
    content: &str,
    expected: Option<&Option<String>>,
    plan: &mut crate::MutationPlan,
) -> Result<(), VaultError> {
    let full = crate::writable_page_path(session.root(), path)?;
    crate::writer::refuse_format_change(&full, content)?;
    let current = match std::fs::read_to_string(&full) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if expected.is_some_and(|expected| expected != &current) {
        return Err(std::io::Error::other(format!(
            "document changed since it was read: {}",
            path.display()
        ))
        .into());
    }
    let after = parse_page(content).map_err(VaultError::Frontmatter)?;
    match current {
        Some(before) => {
            if before == content {
                plan.require_unchanged(path, before);
                return Ok(());
            }
            let old = parse_page(&before).map_err(VaultError::Frontmatter)?;
            if lk_core::concept::source_revision(&old) != lk_core::concept::source_revision(&after)
            {
                let address = path
                    .to_str()
                    .ok_or_else(|| VaultError::Frontmatter("document path is not UTF-8".into()))?;
                let report = archive_evidence(session.root(), &[address.to_owned()])?;
                if report.file_revisions.get(address)
                    != Some(&lk_core::concept::file_revision(&before))
                {
                    return Err(VaultError::Frontmatter(format!(
                        "document changed while archiving: {}",
                        path.display()
                    )));
                }
                let revision = lk_core::concept::source_revision(&old);
                let archived = load_archived_evidence(session.root(), &revision)?
                    .and_then(|page| page.original)
                    .ok_or_else(|| {
                        VaultError::Frontmatter("archived evidence disappeared".into())
                    })?;
                plan.require_unchanged(evidence_archive_path(&revision)?, archived);
            }
            // MutationPlan compares the exact captured bytes again before writing.
            plan.edit(path, before, content.to_owned());
        }
        None => {
            plan.create(path, content.to_owned());
        }
    }
    Ok(())
}

fn publish_archive(root: &Path, revision: &str, raw: &str) -> Result<(), VaultError> {
    // Publish durable bytes through VaultWriter, then a no-clobber hard link.
    // Even concurrent archivers cannot silently replace an existing snapshot.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let destination = crate::writable_page_path(root, &evidence_archive_path(revision)?)?;
    let directory = destination
        .parent()
        .expect("archive has an evidence directory");
    std::fs::create_dir_all(directory)?;
    let staging = loop {
        let candidate = directory.join(format!(
            ".archive-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let result = (|| {
        crate::VaultWriter::new(&staging).write_page_sync(Path::new("snapshot.md"), raw)?;
        match std::fs::hard_link(staging.join("snapshot.md"), &destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                load_archived_evidence(root, revision)?.ok_or_else(|| {
                    VaultError::Frontmatter(
                        "evidence archive disappeared during publication".into(),
                    )
                })?;
            }
            Err(error) => return Err(VaultError::Io(error)),
        }
        #[cfg(unix)]
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    let cleanup = std::fs::remove_dir_all(staging);
    result?;
    cleanup?;
    Ok(())
}

/// Report open assessments and evidence drift without changing a recorded judgment.
pub fn list_reviews(root: &Path, dirs: &VaultDirs) -> Result<Vec<ReviewReport>, VaultError> {
    let mut reports = Vec::new();
    let mut revisions: BTreeMap<String, Result<String, VaultError>> = BTreeMap::new();
    let inventory =
        crate::load_markdown_inventory(root, &lk_core::vault_path::explorations_dir(dirs))?;
    for relative in inventory.owners {
        let path = root.join(relative);
        let mut report = ReviewReport {
            path: path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned(),
            review: None,
            effective_state: ReviewEffectiveState::Invalid,
            changed_evidence: Vec::new(),
            archived_evidence: Vec::new(),
            missing_evidence: Vec::new(),
            errors: Vec::new(),
            scope: ReviewScope::default(),
        };
        let page = match crate::page_path(root, path.strip_prefix(root).unwrap_or(&path))
            .and_then(std::fs::read_to_string)
            .map_err(VaultError::from)
            .and_then(|content| {
                parse_page(&content).map_err(|e| VaultError::Frontmatter(e.to_string()))
            }) {
            Ok(page) => page,
            Err(error) => {
                report.errors.push(error.to_string());
                reports.push(report);
                continue;
            }
        };
        let Some(value) = page.frontmatter.get("review") else {
            continue;
        };
        report.scope = ReviewScope::from_metadata(value);
        for subject in &report.scope.subjects {
            if let Err(error) = crate::find_page_owner(root, Path::new(subject)) {
                report.errors.push(format!("{subject}: {error}"));
            }
        }
        match serde_json::from_value::<KnowledgeReview>(value.clone()) {
            Err(error) => report.errors.push(error.to_string()),
            Ok(review) => {
                report.errors.extend(review.validation_errors(
                    path.strip_prefix(root).expect("review belongs to vault"),
                    &page,
                ));
                for citation in &review.evidence {
                    if lk_core::link::resolve_dest(Path::new(""), &citation.path).as_deref()
                        == path.strip_prefix(root).ok()
                    {
                        report
                            .errors
                            .push("a review cannot serve as its own evidence".into());
                        continue;
                    }
                    let archived = match load_archived_evidence(root, &citation.revision) {
                        Ok(Some(_)) => {
                            report.archived_evidence.push(citation.path.clone());
                            true
                        }
                        Ok(None) => false,
                        Err(error) => {
                            report.errors.push(format!("{}: {error}", citation.path));
                            false
                        }
                    };
                    let revision = revisions.entry(citation.path.clone()).or_insert_with(|| {
                        read_source(root, &citation.path)
                            .map(|page| lk_core::concept::source_revision(&page))
                    });
                    match revision {
                        Ok(revision) if *revision != citation.revision => {
                            report.changed_evidence.push(citation.path.clone())
                        }
                        Ok(_) => {}
                        Err(VaultError::Io(error))
                            if error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            report.missing_evidence.push(citation.path.clone());
                            if !archived {
                                report.errors.push(format!(
                                    "{}: current evidence missing without a verified archive",
                                    citation.path
                                ));
                            }
                        }
                        Err(error) => report.errors.push(format!("{}: {error}", citation.path)),
                    }
                }
                report.review = Some(review);
            }
        }
        report.effective_state = report.classify();
        reports.push(report);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn review_repointing_accepts_symlink_subject_scope_but_keeps_hardlinks_distinct() {
        for hardlink in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let before = "---\ntype: concept\nknowledge_review: review-alias.md\n---\nOld claim.\n";
            std::fs::write(root.path().join("old.md"), before).unwrap();
            let evidence = "---\ntype: concept\n---\nSurvivor.\n";
            std::fs::write(root.path().join("new.md"), evidence).unwrap();
            if hardlink {
                std::fs::hard_link(root.path().join("old.md"), root.path().join("alias.md"))
                    .unwrap();
            } else {
                std::os::unix::fs::symlink("old.md", root.path().join("alias.md")).unwrap();
            }
            let metadata = serde_json::json!({
                "status":"open", "subjects":["alias.md"],
                "evidence":[{"path":"new.md","revision":lk_core::concept::source_revision(&parse_page(evidence).unwrap())}]
            });
            let review = format!("---\ntype: exploration\nreview: {metadata}\n---\nOpen review.\n");
            std::fs::write(root.path().join("review.md"), &review).unwrap();
            std::os::unix::fs::symlink("review.md", root.path().join("review-alias.md")).unwrap();
            let result = plan_review_repointing(
                root.path(),
                &["old.md".into(), "new.md".into(), "review.md".into()],
                &[("old.md".into(), "new.md".into())],
                &mut crate::MutationPlan::default(),
            );
            if hardlink {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("does not declare the moved subject")
                );
            } else {
                let updates = result.unwrap();
                let migrated = parse_page(&updates[Path::new("review.md")].1).unwrap();
                let metadata_after = migrated.frontmatter.get("review").unwrap();
                assert_eq!(metadata_after["subjects"], serde_json::json!(["new.md"]));
                assert_eq!(metadata_after["evidence"], metadata["evidence"]);
            }
            assert_eq!(
                std::fs::read_to_string(root.path().join("review.md")).unwrap(),
                review
            );
        }
    }

    #[test]
    fn unchanged_document_publication_keeps_composed_work_and_guards_target() {
        let root = tempfile::tempdir().unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let document = "---\ntype: document\n---\nUnchanged source.\n";
        std::fs::write(root.path().join("document.md"), document).unwrap();
        let mut plan = crate::MutationPlan::default();
        plan.create("work.json", "pending work".into());
        publish_document_expected_with_plan(
            &session,
            Path::new("document.md"),
            document,
            Some(&Some(document.into())),
            plan,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("work.json")).unwrap(),
            "pending work"
        );
        assert!(!root.path().join(".lorekeeper/evidence").exists());
        let mut plan = crate::MutationPlan::default();
        plan_document_publication(
            &session,
            Path::new("document.md"),
            document,
            Some(&Some(document.into())),
            &mut plan,
        )
        .unwrap();
        plan.create("next-work.json", "next work".into());
        std::fs::write(
            root.path().join("document.md"),
            document.replace("Unchanged", "Changed"),
        )
        .unwrap();
        assert!(session.apply(plan).is_err());
        assert!(!root.path().join("next-work.json").exists());
        assert!(
            !root
                .path()
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn planned_document_edit_and_work_share_recoverable_publication() {
        let root = tempfile::tempdir().unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let before = "---\ntype: document\n---\nOriginal source.\n";
        let after = "---\ntype: document\n---\nUpdated source.\n";
        std::fs::write(root.path().join("document.md"), before).unwrap();
        let mut plan = crate::MutationPlan::default();
        plan_document_publication(
            &session,
            Path::new("document.md"),
            after,
            Some(&Some(before.into())),
            &mut plan,
        )
        .unwrap();
        plan.create("work.json", "pending work".into());
        let journal = serde_json::json!({"version":1,"plan":plan});
        std::fs::write(
            root.path().join(".lorekeeper/mutation/pending.json"),
            journal.to_string(),
        )
        .unwrap();
        std::fs::write(root.path().join("document.md"), after).unwrap();
        drop(session);
        let _session = crate::MutationSession::acquire(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("document.md")).unwrap(),
            after
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("work.json")).unwrap(),
            "pending work"
        );
        let revision = lk_core::concept::source_revision(&parse_page(before).unwrap());
        assert_eq!(
            load_archived_evidence(root.path(), &revision)
                .unwrap()
                .unwrap()
                .original
                .as_deref(),
            Some(before)
        );
        assert!(
            !root
                .path()
                .join(".lorekeeper/mutation/pending.json")
                .exists()
        );
    }

    #[test]
    fn document_publication_preserves_prior_evidence_and_bounds_noops() {
        let root = tempfile::tempdir().unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let path = Path::new("document.md");
        let before =
            "---\ntitle: Source\nsource_record:\n  status: active\n---\nOriginal evidence\n";
        let after =
            "---\ntitle: Source\nsource_record:\n  status: archived\n---\nCorrected evidence\n";
        publish_document(&session, path, before).unwrap();
        assert!(!root.path().join(".lorekeeper/evidence").exists());
        publish_document(&session, path, after).unwrap();
        let revision = lk_core::concept::source_revision(&parse_page(before).unwrap());
        let archived = load_archived_evidence(root.path(), &revision)
            .unwrap()
            .unwrap();
        assert_eq!(archived.body, parse_page(before).unwrap().body);
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            after
        );
        publish_document(&session, path, after).unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join(".lorekeeper/evidence"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn corrupt_prior_archive_prevents_document_replacement() {
        let root = tempfile::tempdir().unwrap();
        let session = crate::MutationSession::acquire(root.path()).unwrap();
        let path = Path::new("document.md");
        let before = "---\ntitle: Source\n---\nOriginal evidence\n";
        publish_document(&session, path, before).unwrap();
        let revision = lk_core::concept::source_revision(&parse_page(before).unwrap());
        let archive = root.path().join(evidence_archive_path(&revision).unwrap());
        std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
        std::fs::write(archive, "corrupt evidence").unwrap();
        assert!(publish_document(&session, path, "---\ntitle: Source\n---\nChanged\n").is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(path)).unwrap(),
            before
        );
    }

    #[cfg(unix)]
    #[test]
    fn evidence_reads_reject_sources_outside_the_vault_through_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("source.md"), "External evidence.").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("alias")).unwrap();
        assert!(inspect_evidence(root.path(), &["alias/source.md".into()]).is_err());
        assert!(archive_evidence(root.path(), &["alias/source.md".into()]).is_err());
        assert!(
            KnowledgeReview::ensure_evidence_paths_preserved(
                root.path(),
                &["alias/source.md".into()],
                &[],
            )
            .is_err()
        );
        let dirs = VaultDirs::default();
        std::fs::create_dir(root.path().join(&dirs.wiki)).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            root.path()
                .join(lk_core::vault_path::explorations_dir(&dirs)),
        )
        .unwrap();
        assert!(list_reviews(root.path(), &dirs).is_err());
        assert!(!root.path().join(".lorekeeper/evidence").exists());
        std::fs::write(root.path().join("local.md"), "Local evidence.").unwrap();
        std::fs::create_dir(root.path().join(".lorekeeper")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".lorekeeper/evidence"))
            .unwrap();
        assert!(archive_evidence(root.path(), &["local.md".into()]).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn removal_checks_evidence_entry_aliases_without_conflating_hard_links() {
        let root = tempfile::tempdir().unwrap();
        let source = "---\ntype: document\n---\nHistorical evidence.\n";
        std::fs::write(root.path().join("old.md"), source).unwrap();
        std::os::unix::fs::symlink("old.md", root.path().join("alias.md")).unwrap();
        std::fs::hard_link(root.path().join("old.md"), root.path().join("distinct.md")).unwrap();
        let revision = lk_core::concept::source_revision(&parse_page(source).unwrap());
        let review = format!(
            "---\ntype: exploration\nreview: {{status: open, evidence: [{{path: alias.md, revision: {revision}}}]}}\n---\nJudgment.\n"
        );
        std::fs::write(root.path().join("review.md"), review).unwrap();
        KnowledgeReview::ensure_evidence_paths_preserved(
            root.path(),
            &["review.md".into()],
            &["distinct.md".into()],
        )
        .unwrap();
        assert!(
            KnowledgeReview::ensure_evidence_paths_preserved(
                root.path(),
                &["review.md".into()],
                &["old.md".into()]
            )
            .is_err()
        );
        archive_evidence(root.path(), &["alias.md".into()]).unwrap();
        KnowledgeReview::ensure_evidence_paths_preserved(
            root.path(),
            &["review.md".into()],
            &["old.md".into()],
        )
        .unwrap();
    }

    #[test]
    fn archive_preserves_past_evidence_and_reports_current_drift_and_missing_sources() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let original = "---\ntype: document\ntitle: Contract\n---\nTimeout is 30 seconds.\n";
        std::fs::write(root.path().join("source.md"), original).unwrap();
        let report = archive_evidence(root.path(), &["source.md".into()]).unwrap();
        let citation = &report.citations[0];
        let archived = root
            .path()
            .join(evidence_archive_path(&citation.revision).unwrap());
        assert_eq!(std::fs::read_to_string(&archived).unwrap(), original);
        let directory = root
            .path()
            .join(lk_core::vault_path::explorations_dir(&dirs));
        std::fs::create_dir_all(&directory).unwrap();
        let review = serde_json::json!({"status":"open", "evidence":report.citations});
        std::fs::write(
            directory.join("review.md"),
            format!("---\ntype: exploration\nreview: {review}\n---\n"),
        )
        .unwrap();
        std::fs::write(
            root.path().join("source.md"),
            original.replace("30 seconds", "5 seconds"),
        )
        .unwrap();
        let reports = list_reviews(root.path(), &dirs).unwrap();
        assert_eq!(reports[0].archived_evidence, vec!["source.md"]);
        assert_eq!(reports[0].changed_evidence, vec!["source.md"]);
        assert!(reports[0].errors.is_empty());
        let page = load_archived_evidence(root.path(), &citation.revision)
            .unwrap()
            .unwrap();
        assert!(page.body.contains("30 seconds"));
        KnowledgeReview::ensure_evidence_paths_preserved(
            root.path(),
            &[directory
                .join("review.md")
                .strip_prefix(root.path())
                .unwrap()
                .to_path_buf()],
            &["source.md".into()],
        )
        .unwrap();
        std::fs::remove_file(root.path().join("source.md")).unwrap();
        let reports = list_reviews(root.path(), &dirs).unwrap();
        assert_eq!(reports[0].archived_evidence, vec!["source.md"]);
        assert_eq!(reports[0].missing_evidence, vec!["source.md"]);
        assert!(reports[0].errors.is_empty());
        assert_eq!(reports[0].effective_state, ReviewEffectiveState::Open);
        assert!(!reports[0].effective_state.requires_attention());
        std::fs::write(&archived, original.replace("30 seconds", "tampered")).unwrap();
        assert!(load_archived_evidence(root.path(), &citation.revision).is_err());
        let reports = list_reviews(root.path(), &dirs).unwrap();
        assert!(reports[0].archived_evidence.is_empty());
        assert!(
            reports[0]
                .errors
                .iter()
                .any(|error| error.contains("corrupt evidence archive"))
        );
        assert!(
            KnowledgeReview::ensure_evidence_paths_preserved(
                root.path(),
                &[directory
                    .join("review.md")
                    .strip_prefix(root.path())
                    .unwrap()
                    .to_path_buf()],
                &["source.md".into()]
            )
            .is_err()
        );
    }

    #[test]
    fn archiving_validates_whole_batch_and_never_overwrites_existing_content() {
        let root = tempfile::tempdir().unwrap();
        let original = "---\ntitle: Contract\nupdated: old\n---\nFact.\n";
        std::fs::write(root.path().join("a.md"), original).unwrap();
        assert!(archive_evidence(root.path(), &["a.md".into(), "missing.md".into()]).is_err());
        assert!(!root.path().join(".lorekeeper/evidence").exists());
        let first = archive_evidence(root.path(), &["a.md".into()]).unwrap();
        let archive = root
            .path()
            .join(evidence_archive_path(&first.citations[0].revision).unwrap());
        std::fs::write(
            root.path().join("a.md"),
            original.replace("updated: old", "updated: new"),
        )
        .unwrap();
        archive_evidence(root.path(), &["a.md".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), original);
        std::fs::write(&archive, "corruption").unwrap();
        assert!(archive_evidence(root.path(), &["a.md".into()]).is_err());
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), "corruption");
        assert!(load_archived_evidence(root.path(), "../../outside").is_err());
        assert!(
            load_archived_evidence(root.path(), &"0".repeat(32))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn destructive_preflight_requires_parseable_pages_without_text_heuristics() {
        let root = tempfile::tempdir().unwrap();
        let pages = vec!["review.md".into()];
        let removed = vec!["wiki/old.md".into()];
        for raw in [
            "---\n\"review\": {status: open, evidence: [{path: wiki/old.md, revision: past}]}\n---\n",
            "---\nreview: [\n---\n",
            "---\ntitle: unclosed\n",
        ] {
            std::fs::write(root.path().join("review.md"), raw).unwrap();
            assert!(
                KnowledgeReview::ensure_evidence_paths_preserved(root.path(), &pages, &removed)
                    .is_err()
            );
        }
        std::fs::write(
            root.path().join("review.md"),
            "---\ntitle: Ordinary\n---\nreview: this is body prose, not metadata\n",
        )
        .unwrap();
        KnowledgeReview::ensure_evidence_paths_preserved(root.path(), &pages, &removed).unwrap();
        std::fs::write(root.path().join("review.md"), [0xff]).unwrap();
        assert!(
            KnowledgeReview::ensure_evidence_paths_preserved(root.path(), &pages, &removed)
                .is_err()
        );
    }

    #[test]
    fn malformed_explorations_do_not_hide_valid_reviews() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        let directory = root
            .path()
            .join(lk_core::vault_path::explorations_dir(&dirs));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("bad.md"), "---\nreview: [\n---\n").unwrap();
        std::fs::write(directory.join("unreadable.md"), [0xff, 0xfe]).unwrap();
        std::fs::write(
            directory.join("valid.md"),
            "---\ntype: exploration\nreview: {status: open, evidence: []}\n---\n",
        )
        .unwrap();
        let reports = list_reviews(root.path(), &dirs).unwrap();
        assert_eq!(reports.len(), 3);
        assert!(reports[0].review.is_none());
        assert!(!reports[0].errors.is_empty());
        assert!(!reports[1].errors.is_empty());
        assert!(reports[2].review.is_some());
        assert!(reports.iter().all(|report| {
            report.effective_state == ReviewEffectiveState::Invalid
                && report.effective_state.requires_attention()
        }));
    }

    #[test]
    fn historical_evidence_blocks_removal_without_rewriting_revision() {
        let root = tempfile::tempdir().unwrap();
        let raw = "---\ntype: exploration\nreview: {status: resolved, evidence: [{path: wiki/old.md, revision: historical}], resolution: compared}\n---\n";
        std::fs::write(root.path().join("review.md"), raw).unwrap();
        let pages = vec!["review.md".into()];
        assert!(
            KnowledgeReview::ensure_evidence_paths_preserved(
                root.path(),
                &pages,
                &["wiki/old.md".into()]
            )
            .is_err()
        );
        KnowledgeReview::ensure_evidence_paths_preserved(
            root.path(),
            &pages,
            &["wiki/other.md".into()],
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("review.md")).unwrap(),
            raw
        );
    }

    #[test]
    fn recorded_origins_and_review_evidence_do_not_claim_independence_or_silent_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        for name in ["a.md", "b.md"] {
            std::fs::write(
                dir.path().join(name),
                "---\nsource_url: https://example.com/spec\n---\nTimeout is 30 seconds.\n",
            )
            .unwrap();
        }
        let evidence = inspect_evidence(dir.path(), &["a.md".into(), "b.md".into()]).unwrap();
        assert_eq!(evidence.origins.len(), 1);
        assert_eq!(evidence.citations.len(), 2);
        let path = dir
            .path()
            .join(lk_core::vault_path::explorations_dir(&dirs));
        std::fs::create_dir_all(&path).unwrap();
        let review = serde_json::json!({"status": "resolved", "evidence": evidence.citations, "resolution": "Different environments."});
        std::fs::write(
            path.join("timeout.md"),
            format!(
                "---\ntype: exploration\nreview: {review}\n---\nCompared production with staging.\n"
            ),
        )
        .unwrap();
        assert_eq!(
            list_reviews(dir.path(), &dirs).unwrap()[0].effective_state,
            ReviewEffectiveState::Resolved
        );
        assert!(
            list_reviews(dir.path(), &dirs).unwrap()[0]
                .changed_evidence
                .is_empty()
        );
        std::fs::write(
            dir.path().join("a.md"),
            "---\nsource_url: https://example.com/spec\n---\nTimeout is 5 seconds.\n",
        )
        .unwrap();
        let reports = list_reviews(dir.path(), &dirs).unwrap();
        assert_eq!(reports[0].changed_evidence, vec!["a.md"]);
        assert_eq!(
            reports[0].effective_state,
            ReviewEffectiveState::NeedsReassessment
        );
        assert!(reports[0].effective_state.requires_attention());
        assert_eq!(
            reports[0].review.as_ref().unwrap().status,
            ReviewStatus::Resolved
        );
        std::fs::remove_file(dir.path().join("b.md")).unwrap();
        let reports = list_reviews(dir.path(), &dirs).unwrap();
        assert_eq!(reports[0].missing_evidence, vec!["b.md"]);
        assert_eq!(reports[0].errors.len(), 1);
        assert_eq!(reports[0].effective_state, ReviewEffectiveState::Invalid);
    }

    #[test]
    fn explicit_reassessment_can_replace_current_evidence_without_losing_prior_judgment() {
        let root = tempfile::tempdir().unwrap();
        let dirs = VaultDirs::default();
        std::fs::write(
            root.path().join("source.md"),
            "---\ntype: document\n---\nOld claim.\n",
        )
        .unwrap();
        let old_evidence = archive_evidence(root.path(), &["source.md".into()]).unwrap();
        let review_path = lk_core::vault_path::explorations_dir(&dirs).join("review.md");
        std::fs::create_dir_all(root.path().join(review_path.parent().unwrap())).unwrap();
        let review = serde_json::json!({"status":"resolved", "evidence":old_evidence.citations,"resolution":"Initial judgment."});
        let old_review =
            format!("---\ntype: exploration\nreview: {review}\n---\nOriginal judgment.\n");
        std::fs::write(root.path().join(&review_path), &old_review).unwrap();
        let prior =
            archive_evidence(root.path(), &[review_path.to_string_lossy().into_owned()]).unwrap();
        let prior_revision = &prior.citations[0].revision;
        std::fs::write(
            root.path().join("source.md"),
            "---\ntype: document\n---\nNew claim.\n",
        )
        .unwrap();
        assert_eq!(
            list_reviews(root.path(), &dirs).unwrap()[0].effective_state,
            ReviewEffectiveState::NeedsReassessment
        );
        let current = archive_evidence(root.path(), &["source.md".into()]).unwrap();
        let reassessed = serde_json::json!({"status":"resolved", "evidence":current.citations,"resolution":"Reassessed changed evidence."});
        std::fs::write(root.path().join(review_path), format!("---\ntype: exploration\nreview: {reassessed}\n---\n2026-10-17: reassessed; prior judgment: .lorekeeper/evidence/{prior_revision}.md\n")).unwrap();
        let reports = list_reviews(root.path(), &dirs).unwrap();
        assert_eq!(reports[0].effective_state, ReviewEffectiveState::Resolved);
        assert!(reports[0].changed_evidence.is_empty());
        let preserved = load_archived_evidence(root.path(), prior_revision)
            .unwrap()
            .unwrap();
        assert_eq!(preserved.frontmatter.get("review").unwrap(), &review);
        assert!(preserved.body.contains("Original judgment."));
        assert!(
            load_archived_evidence(root.path(), &old_evidence.citations[0].revision)
                .unwrap()
                .is_some()
        );
    }
}
