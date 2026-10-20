use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

const CURSOR: &str = ".lorekeeper/execution-cursor.json";

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Queue,
    Audit,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Cursor {
    queue: Option<String>,
    audit: Option<String>,
}

/// Reserve one round-robin turn before starting its external worker. A cancelled
/// worker keeps its work but does not monopolize the next resumed run.
pub(super) fn select(
    root: &Path,
    stage: Stage,
    candidates: &BTreeSet<String>,
) -> miette::Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let session = lk_vault::MutationSession::acquire(root).into_diagnostic()?;
    let path = PathBuf::from(CURSOR);
    let full = lk_vault::page_path(session.root(), &path).into_diagnostic()?;
    let before = match std::fs::read_to_string(full) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(miette::miette!("execution cursor: {error}")),
    };
    let mut cursor: Cursor = before
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .into_diagnostic()?
        .unwrap_or_default();
    let next = match stage {
        Stage::Queue => &mut cursor.queue,
        Stage::Audit => &mut cursor.audit,
    };
    let selected = next
        .as_ref()
        .and_then(|next| candidates.range(next.clone()..).next())
        .or_else(|| candidates.first())
        .expect("nonempty candidates")
        .clone();
    *next = candidates
        .range((
            std::ops::Bound::Excluded(selected.clone()),
            std::ops::Bound::Unbounded,
        ))
        .next()
        .or_else(|| candidates.first())
        .cloned();
    let mut mutation = lk_vault::MutationPlan::default();
    mutation
        .publish_expected(
            session.root(),
            path,
            before,
            serde_json::to_string(&cursor).into_diagnostic()?,
        )
        .into_diagnostic()?;
    session.apply(mutation).into_diagnostic()?;
    Ok(Some(selected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_survive_restarts_and_removed_targets_without_sharing_queue_and_audit_order() {
        let root = tempfile::tempdir().unwrap();
        let candidates = ["a".into(), "b".into()].into();
        assert_eq!(
            select(root.path(), Stage::Queue, &candidates)
                .unwrap()
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            select(root.path(), Stage::Audit, &candidates)
                .unwrap()
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            select(root.path(), Stage::Queue, &candidates)
                .unwrap()
                .as_deref(),
            Some("b")
        );
        let changed = ["c".into(), "d".into()].into();
        assert_eq!(
            select(root.path(), Stage::Queue, &changed)
                .unwrap()
                .as_deref(),
            Some("c")
        );
        assert_eq!(
            select(root.path(), Stage::Queue, &changed)
                .unwrap()
                .as_deref(),
            Some("d")
        );
        assert_eq!(
            select(root.path(), Stage::Queue, &changed)
                .unwrap()
                .as_deref(),
            Some("c")
        );
    }

    #[test]
    fn invalid_state_is_not_reset_and_empty_selection_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            select(root.path(), Stage::Queue, &BTreeSet::new())
                .unwrap()
                .is_none()
        );
        assert!(!root.path().join(".lorekeeper").exists());
        std::fs::create_dir(root.path().join(".lorekeeper")).unwrap();
        std::fs::write(root.path().join(CURSOR), "invalid").unwrap();
        assert!(select(root.path(), Stage::Queue, &["a".into()].into()).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(CURSOR)).unwrap(),
            "invalid"
        );
    }

    #[test]
    fn new_arrivals_do_not_displace_the_already_reserved_next_target() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            select(root.path(), Stage::Queue, &["a".into(), "z".into()].into())
                .unwrap()
                .as_deref(),
            Some("a")
        );
        let expanded = ["a".into(), "m".into(), "z".into()].into();
        assert_eq!(
            select(root.path(), Stage::Queue, &expanded)
                .unwrap()
                .as_deref(),
            Some("z")
        );
        assert_eq!(
            select(root.path(), Stage::Queue, &expanded)
                .unwrap()
                .as_deref(),
            Some("a")
        );
    }
}
