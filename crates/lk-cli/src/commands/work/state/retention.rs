use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use lk_vault::{MutationPlan, MutationSession};
use serde_json::Value;

use super::{
    Publication, Seal, StoredOutput, Work, fail, output_path, read_optional, require_id,
    require_settled, revision, work_path,
};

#[derive(Default)]
pub(crate) struct WorkRetention {
    pub removed: usize,
    pub retained: usize,
    pub bytes: usize,
}

struct RetentionCapture {
    id: String,
    revisions: BTreeMap<PathBuf, String>,
    bytes: usize,
    references: BTreeSet<String>,
    expired: bool,
}

fn capture_retention(root: &Path, id: &str, cutoff: i64) -> miette::Result<RetentionCapture> {
    let work = Work::load(root, id)?;
    let directory = work_path(id, "")?;
    let mut known = [
        "plan.json",
        "seal.json",
        "final.json",
        "assessment.json",
        "publication.json",
    ]
    .into_iter()
    .map(|name| work_path(id, name))
    .collect::<miette::Result<BTreeSet<_>>>()?;
    let nodes = work.nodes()?;
    for node in &nodes {
        known.insert(output_path(id, &node.id)?);
    }
    let mut revisions = BTreeMap::new();
    let mut bytes = 0;
    let mut references = BTreeSet::new();
    let mut unknown = false;
    for entry in walkdir::WalkDir::new(root.join(&directory))
        .max_depth(2)
        .follow_links(false)
    {
        let entry = entry.map_err(fail)?;
        if entry.file_type().is_symlink() {
            return Err(fail("work retention refuses symlink artifacts"));
        }
        let path = entry.path().strip_prefix(root).map_err(fail)?.to_path_buf();
        if path == directory || (path == directory.join("nodes") && entry.file_type().is_dir()) {
            continue;
        }
        if !known.contains(&path) {
            unknown = true;
            continue;
        }
        let raw = read_optional(root, &path)?
            .ok_or_else(|| fail("work artifact disappeared during retention capture"))?;
        if path.parent() == Some(directory.join("nodes").as_path()) {
            let stored: StoredOutput = serde_json::from_str(&raw).map_err(fail)?;
            if stored.output.work_id != id
                || Some(stored.output.node_id.as_str())
                    != path.file_stem().and_then(|name| name.to_str())
                || revision(&stored.output)? != stored.output_revision
            {
                return Err(fail("invalid retained work observation"));
            }
            if let Some(reused) = stored.reused_from {
                require_id(&reused.work_id)?;
                references.insert(reused.work_id);
            }
        }
        bytes += raw.len();
        revisions.insert(path, lk_core::concept::file_revision(&raw));
    }
    let read = |name| -> miette::Result<Option<String>> {
        let path = work_path(id, name)?;
        match revisions.get(&path) {
            Some(expected) => read_captured(root, &path, expected).map(Some),
            None => Ok(None),
        }
    };
    let expired = if let Some(raw) = read("publication.json")? {
        let receipt: Publication = serde_json::from_str(&raw).map_err(fail)?;
        let seal: Seal = serde_json::from_str(
            &read("seal.json")?.ok_or_else(|| fail("published work seal is missing"))?,
        )
        .map_err(fail)?;
        let result: Value = serde_json::from_str(
            &read("final.json")?.ok_or_else(|| fail("published work result is missing"))?,
        )
        .map_err(fail)?;
        if receipt.work_id != id
            || seal.work_id != id
            || receipt.final_result_revision != seal.final_result_revision
            || revision(&result)? != receipt.final_result_revision
        {
            return Err(fail("published work receipt, seal or result differs"));
        }
        !unknown && receipt.published_at.as_second() < cutoff
    } else {
        false
    };
    Ok(RetentionCapture {
        id: id.to_owned(),
        revisions,
        bytes,
        references,
        expired,
    })
}

fn read_captured(root: &Path, path: &Path, expected: &str) -> miette::Result<String> {
    let raw = read_optional(root, path)?.ok_or_else(|| fail("work retention input disappeared"))?;
    if lk_core::concept::file_revision(&raw) != expected {
        return Err(fail("work retention input changed"));
    }
    Ok(raw)
}

/// Caller holds the workflow lease and mutation session when pruning. Unknown,
/// unpublished and referenced work survives; no source or knowledge file is owned here.
pub(crate) fn prune_published(
    root: &Path,
    session: Option<&MutationSession>,
    cutoff: i64,
) -> miette::Result<WorkRetention> {
    require_settled(root)?;
    let directory = lk_vault::page_path(root, Path::new(".lorekeeper/work")).map_err(fail)?;
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkRetention::default());
        }
        Err(error) => return Err(fail(error)),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(fail("work store must be a direct directory"));
    }
    let root_owned = root.canonicalize().map_err(fail)?;
    let root = root_owned.as_path();
    let mut captures = BTreeMap::new();
    for entry in std::fs::read_dir(directory).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let id = entry
            .file_name()
            .into_string()
            .map_err(|_| fail("work address is not UTF-8"))?;
        require_id(&id)?;
        if !entry.file_type().map_err(fail)?.is_dir() {
            return Err(fail("work entry must be a direct directory"));
        }
        captures.insert(id.clone(), capture_retention(root, &id, cutoff)?);
    }
    let mut retained = captures
        .values()
        .filter(|work| !work.expired)
        .map(|work| work.id.clone())
        .collect::<BTreeSet<_>>();
    let mut pending = retained.iter().cloned().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if let Some(work) = captures.get(&id) {
            for reference in &work.references {
                if retained.insert(reference.clone()) {
                    pending.push(reference.clone());
                }
            }
        }
    }
    if session.is_some_and(|session| session.root() != root) {
        return Err(fail("retention session belongs to another vault"));
    }
    let mut report = WorkRetention::default();
    for work in captures.values() {
        if retained.contains(&work.id) {
            report.retained += 1;
            continue;
        }
        report.removed += 1;
        report.bytes += work.bytes;
        if let Some(session) = session {
            let current = capture_retention(root, &work.id, cutoff)?;
            if current.revisions != work.revisions || !current.expired {
                return Err(fail("work retention inventory changed"));
            }
            let mut mutation = MutationPlan::default();
            for (path, digest) in &work.revisions {
                mutation.remove(path, read_captured(root, path, digest)?);
            }
            session.apply(mutation).map_err(fail)?;
            for suffix in ["nodes", ""] {
                match std::fs::remove_dir(root.join(work_path(&work.id, suffix)?)) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(error) => return Err(fail(error)),
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::super::encoded;
    use super::super::tests::{context, finish, limits, resources};
    use super::*;
    use lk_core::semantic::SemanticResource;
    use serde_json::json;

    fn publish(work: &Work, session: &MutationSession, resources: &[SemanticResource]) {
        finish(work, session, resources);
        let result = json!({"body":"Published typed result"});
        work.seal(session, resources, result.clone()).unwrap();
        let mut plan = MutationPlan::default();
        work.guard_completion(&mut plan, session.root(), resources, &result)
            .unwrap();
        work.plan_publication(&mut plan, session.root(), &result)
            .unwrap();
        plan.publish_expected(
            session.root(),
            work_path(&work.plan.id, "final.json").unwrap(),
            None,
            encoded(&result).unwrap(),
        )
        .unwrap();
        session.apply(plan).unwrap();
    }

    #[test]
    fn retention_requires_published_receipts_and_preserves_pending_and_sealed_work() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let published = Work::create(
            &session,
            &source,
            "published",
            context("published"),
            limits(),
        )
        .unwrap();
        let sealed =
            Work::create(&session, &source, "sealed", context("sealed"), limits()).unwrap();
        let pending =
            Work::create(&session, &source, "pending", context("pending"), limits()).unwrap();
        finish(&sealed, &session, &source);
        sealed
            .seal(&session, &source, json!({"body":"not published"}))
            .unwrap();
        publish(&published, &session, &source);
        let cutoff = jiff::Timestamp::now().as_second() + 1;
        assert_eq!(prune_published(root.path(), None, 0).unwrap().removed, 0);
        let preview = prune_published(root.path(), None, cutoff).unwrap();
        assert_eq!(preview.removed, 1);
        assert_eq!(preview.retained, 2);
        assert!(preview.bytes > 0);
        assert!(
            root.path()
                .join(work_path(&published.plan.id, "plan.json").unwrap())
                .exists()
        );
        let actual = prune_published(root.path(), Some(&session), cutoff).unwrap();
        assert_eq!(actual.bytes, preview.bytes);
        assert!(
            !root
                .path()
                .join(work_path(&published.plan.id, "").unwrap())
                .exists()
        );
        assert!(Work::load(root.path(), &sealed.plan.id).is_ok());
        assert!(Work::load(root.path(), &pending.plan.id).is_ok());
    }

    #[test]
    fn retention_keeps_reuse_dependencies_and_unowned_files_and_refuses_corruption() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let old = Work::create(&session, &source, "old", context("old"), limits()).unwrap();
        publish(&old, &session, &source);
        let current = Work::create(&session, &source, "new", context("new"), limits()).unwrap();
        current
            .reuse_from(
                &session,
                &old,
                &old.plan.parts[0].id,
                &current.plan.parts[0].id,
                &source,
            )
            .unwrap();
        let independent = Work::create(
            &session,
            &source,
            "independent",
            context("independent"),
            limits(),
        )
        .unwrap();
        publish(&independent, &session, &source);
        let cutoff = jiff::Timestamp::now().as_second() + 1;
        let note = root
            .path()
            .join(work_path(&independent.plan.id, "authored-note.txt").unwrap());
        std::fs::write(&note, "Keep this unowned file.").unwrap();
        assert_eq!(
            prune_published(root.path(), Some(&session), cutoff)
                .unwrap()
                .removed,
            0
        );
        assert!(Work::load(root.path(), &old.plan.id).is_ok());
        std::fs::remove_file(note).unwrap();
        let receipt = root
            .path()
            .join(work_path(&old.plan.id, "publication.json").unwrap());
        let valid = std::fs::read_to_string(&receipt).unwrap();
        std::fs::write(&receipt, "{").unwrap();
        assert!(prune_published(root.path(), Some(&session), cutoff).is_err());
        assert!(Work::load(root.path(), &independent.plan.id).is_ok());
        std::fs::write(receipt, valid).unwrap();
        let report = prune_published(root.path(), Some(&session), cutoff).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.retained, 2);
        assert!(Work::load(root.path(), &old.plan.id).is_ok());
        assert!(Work::load(root.path(), &current.plan.id).is_ok());
    }

    #[test]
    fn publication_receipt_shares_the_canonical_mutations_preflight() {
        let root = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let source = resources();
        let work =
            Work::create(&session, &source, "subject", context("subject"), limits()).unwrap();
        finish(&work, &session, &source);
        let result = json!({"body":"candidate"});
        work.seal(&session, &source, result.clone()).unwrap();
        let mut plan = MutationPlan::default();
        work.guard_completion(&mut plan, root.path(), &source, &result)
            .unwrap();
        work.plan_publication(&mut plan, root.path(), &result)
            .unwrap();
        plan.require_unchanged(
            "wiki/documents/changed.md",
            "expected prior evidence".into(),
        );
        assert!(session.apply(plan).is_err());
        assert!(
            !root
                .path()
                .join(work_path(&work.plan.id, "publication.json").unwrap())
                .exists()
        );
    }
}
