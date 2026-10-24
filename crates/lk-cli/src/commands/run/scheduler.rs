use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

const CURSOR: &str = ".lorekeeper/execution-cursor.json";

#[derive(Clone, Copy, PartialEq, Eq)]
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

fn load(root: &Path) -> miette::Result<(Option<String>, Cursor)> {
    let full = lk_vault::page_path(root, Path::new(CURSOR)).into_diagnostic()?;
    let before = match std::fs::read_to_string(full) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(miette::miette!("execution cursor: {error}")),
    };
    let cursor = before
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .into_diagnostic()?
        .unwrap_or_default();
    Ok((before, cursor))
}

fn ordered(cursor: &Cursor, stage: Stage, candidates: &BTreeSet<String>) -> Vec<String> {
    let next = match stage {
        Stage::Queue => &cursor.queue,
        Stage::Audit => &cursor.audit,
    };
    let mut values = candidates.iter().cloned().collect::<Vec<_>>();
    if let Some(start) = next
        .as_ref()
        .and_then(|next| candidates.range(next.clone()..).next())
    {
        let index = values.binary_search(start).expect("candidate exists");
        values.rotate_left(index);
    }
    values
}

pub(super) fn order(
    root: &Path,
    stage: Stage,
    candidates: &BTreeSet<String>,
) -> miette::Result<Vec<String>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    Ok(ordered(&load(root)?.1, stage, candidates))
}

#[cfg(test)]
pub(super) fn reserve(
    root: &Path,
    stage: Stage,
    candidates: &BTreeSet<String>,
    expected: &[String],
) -> miette::Result<()> {
    if expected.is_empty() {
        return Ok(());
    }
    let session = lk_vault::MutationSession::acquire(root).into_diagnostic()?;
    reserve_in_session(&session, stage, candidates, expected)
}

pub(super) fn reserve_in_session(
    session: &lk_vault::MutationSession,
    stage: Stage,
    candidates: &BTreeSet<String>,
    expected: &[String],
) -> miette::Result<()> {
    if expected.is_empty() {
        return Ok(());
    }
    let (before, mut cursor) = load(session.root())?;
    let order = ordered(&cursor, stage, candidates);
    if order.is_empty()
        || expected
            .iter()
            .zip(order.iter().cycle())
            .any(|(expected, actual)| expected != actual)
    {
        return Err(miette::miette!(
            "execution turn changed during preparation; unfinished work retained"
        ));
    }
    let next = Some(order[expected.len() % order.len()].clone());
    match stage {
        Stage::Queue => cursor.queue = next,
        Stage::Audit => cursor.audit = next,
    }
    let mut mutation = lk_vault::MutationPlan::default();
    mutation
        .publish_expected(
            session.root(),
            PathBuf::from(CURSOR),
            before,
            serde_json::to_string(&cursor).into_diagnostic()?,
        )
        .into_diagnostic()?;
    session.apply(mutation).into_diagnostic()
}

/// Reserve one round-robin turn before starting its external worker. A cancelled
/// worker keeps its work but does not monopolize the next resumed run.
#[cfg(test)]
pub(super) fn select(
    root: &Path,
    stage: Stage,
    candidates: &BTreeSet<String>,
) -> miette::Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let session = lk_vault::MutationSession::acquire(root).into_diagnostic()?;
    select_in_session(&session, stage, candidates)
}

pub(super) fn select_in_session(
    session: &lk_vault::MutationSession,
    stage: Stage,
    candidates: &BTreeSet<String>,
) -> miette::Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let selected = order(session.root(), stage, candidates)?
        .into_iter()
        .next()
        .expect("nonempty candidates");
    reserve_in_session(session, stage, candidates, std::slice::from_ref(&selected))?;
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

    #[test]
    fn planning_order_is_read_only_and_legacy_next_target_keeps_lower_bound_meaning() {
        let root = tempfile::tempdir().unwrap();
        let keys = ["a".into(), "m".into(), "z".into()].into();
        assert_eq!(
            order(root.path(), Stage::Queue, &keys).unwrap(),
            ["a", "m", "z"]
        );
        assert!(!root.path().join(".lorekeeper").exists());
        std::fs::create_dir(root.path().join(".lorekeeper")).unwrap();
        let raw = "{\"queue\":\"k\",\"audit\":\"old\"}";
        std::fs::write(root.path().join(CURSOR), raw).unwrap();
        assert_eq!(
            order(root.path(), Stage::Queue, &keys).unwrap(),
            ["m", "z", "a"]
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(CURSOR)).unwrap(),
            raw
        );
    }

    #[test]
    fn wrapped_batch_reserves_only_consecutive_actual_turns_and_rejects_stale_order() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".lorekeeper")).unwrap();
        std::fs::write(
            root.path().join(CURSOR),
            "{\"queue\":\"z\",\"audit\":\"keep\"}",
        )
        .unwrap();
        let keys = ["a".into(), "m".into(), "z".into()].into();
        reserve(root.path(), Stage::Queue, &keys, &["z".into(), "a".into()]).unwrap();
        assert_eq!(
            order(root.path(), Stage::Queue, &keys).unwrap(),
            ["m", "z", "a"]
        );
        let before = std::fs::read_to_string(root.path().join(CURSOR)).unwrap();
        let cursor: Cursor = serde_json::from_str(&before).unwrap();
        assert_eq!(cursor.audit.as_deref(), Some("keep"));
        assert!(reserve(root.path(), Stage::Queue, &keys, &["z".into()]).is_err());
        assert!(reserve(root.path(), Stage::Queue, &keys, &["m".into(), "a".into()]).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(CURSOR)).unwrap(),
            before
        );
    }
}
