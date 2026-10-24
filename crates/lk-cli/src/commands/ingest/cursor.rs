use std::collections::BTreeSet;
use std::path::Path;

use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

const CURSOR: &str = ".lorekeeper/ingest-cursor.json";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    next_source: String,
}

fn load(root: &Path) -> miette::Result<(Option<String>, Option<Cursor>)> {
    let path = lk_vault::page_path(root, Path::new(CURSOR)).into_diagnostic()?;
    let before = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(miette::miette!("ingest cursor: {error}")),
    };
    let cursor: Option<Cursor> = before
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .into_diagnostic()?;
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.next_source.trim().is_empty())
    {
        return Err(miette::miette!("ingest cursor has a blank next source"));
    }
    Ok((before, cursor))
}

/// Reserve the next run's first admission turn before acquisition. Concurrent
/// collectors read and advance this state under the same short mutation session;
/// completion never writes a stale cursor back over another collector's turn.
pub(super) async fn reserve_start(
    root: &Path,
    candidates: &BTreeSet<String>,
    dry_run: bool,
    wait: bool,
    mut validate_config: impl FnMut() -> miette::Result<()>,
) -> miette::Result<Option<String>> {
    if candidates.is_empty() {
        return Ok(None);
    }
    let session = if dry_run {
        None
    } else {
        std::fs::create_dir_all(root).into_diagnostic()?;
        Some(
            super::super::execution::acquire_mutation_checked(root, wait, &mut validate_config)
                .await?,
        )
    };
    validate_config()?;
    let root = session.as_ref().map_or(root, |session| session.root());
    let (before, cursor) = load(root)?;
    let selected = cursor
        .as_ref()
        .and_then(|cursor| candidates.range(cursor.next_source.clone()..).next())
        .or_else(|| candidates.first())
        .expect("nonempty candidates")
        .clone();
    if let Some(session) = &session {
        let next_source = candidates
            .range((
                std::ops::Bound::Excluded(selected.clone()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .or_else(|| candidates.first())
            .expect("nonempty candidates")
            .clone();
        let mut mutation = lk_vault::MutationPlan::default();
        mutation
            .publish_expected(
                root,
                CURSOR,
                before,
                serde_json::to_string(&Cursor { next_source }).into_diagnostic()?,
            )
            .into_diagnostic()?;
        session.apply(mutation).into_diagnostic()?;
    }
    Ok(Some(selected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserve(
        root: &Path,
        candidates: &BTreeSet<String>,
        dry_run: bool,
        wait: bool,
        validate: impl FnMut() -> miette::Result<()>,
    ) -> miette::Result<Option<String>> {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(super::reserve_start(
                root, candidates, dry_run, wait, validate,
            ))
    }

    fn select(root: &Path, candidates: &[&str], dry_run: bool) -> miette::Result<Option<String>> {
        reserve(
            root,
            &candidates.iter().map(|id| (*id).to_owned()).collect(),
            dry_run,
            false,
            || Ok(()),
        )
    }

    #[test]
    fn turns_survive_failure_restarts_and_changed_enabled_sources() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            select(root.path(), &["a", "z"], false).unwrap().as_deref(),
            Some("a")
        );
        // No completion is necessary to preserve the next source's turn. A new
        // source before the reserved ID does not displace that reservation.
        assert_eq!(
            select(root.path(), &["a", "m", "z"], false)
                .unwrap()
                .as_deref(),
            Some("z")
        );
        assert_eq!(
            select(root.path(), &["m", "n"], false).unwrap().as_deref(),
            Some("m")
        );
        assert_eq!(
            select(root.path(), &["m", "n"], false).unwrap().as_deref(),
            Some("n")
        );
        assert_eq!(
            select(root.path(), &["a"], false).unwrap().as_deref(),
            Some("a")
        );
    }

    #[test]
    fn empty_and_preview_selections_do_not_create_or_change_state() {
        let root = tempfile::tempdir().unwrap();
        assert!(select(root.path(), &[], false).unwrap().is_none());
        assert_eq!(
            select(root.path(), &["a", "b"], true).unwrap().as_deref(),
            Some("a")
        );
        assert!(!root.path().join(".lorekeeper").exists());
        select(root.path(), &["a", "b"], false).unwrap();
        let before = std::fs::read(root.path().join(CURSOR)).unwrap();
        assert_eq!(
            select(root.path(), &["a", "b"], true).unwrap().as_deref(),
            Some("b")
        );
        assert_eq!(std::fs::read(root.path().join(CURSOR)).unwrap(), before);
    }

    #[test]
    fn malformed_state_and_config_drift_refuse_without_resetting_the_cursor() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".lorekeeper")).unwrap();
        let path = root.path().join(CURSOR);
        for invalid in [
            "invalid",
            "{}",
            "{\"next_source\":\"\"}",
            "{\"next_source\":\"a\",\"unknown\":true}",
        ] {
            std::fs::write(&path, invalid).unwrap();
            for dry_run in [true, false] {
                assert!(select(root.path(), &["a", "b"], dry_run).is_err());
                assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
            }
        }
        std::fs::write(&path, "{\"next_source\":\"b\"}").unwrap();
        assert!(
            reserve(
                root.path(),
                &["a".into(), "b".into()].into(),
                false,
                false,
                || Err(miette::miette!("config drift"))
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "{\"next_source\":\"b\"}"
        );
    }

    #[test]
    fn reservation_recovers_pending_state_before_loading_and_preview_does_not_recover() {
        let root = tempfile::tempdir().unwrap();
        select(root.path(), &["a", "b", "c"], false).unwrap();
        let before = std::fs::read_to_string(root.path().join(CURSOR)).unwrap();
        let mut pending = lk_vault::MutationPlan::default();
        pending.edit(CURSOR, before.clone(), "{\"next_source\":\"c\"}".into());
        let journal = root.path().join(".lorekeeper/mutation/pending.json");
        std::fs::write(
            &journal,
            serde_json::to_vec(&serde_json::json!({"version":1,"plan":pending})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            select(root.path(), &["a", "b", "c"], true)
                .unwrap()
                .as_deref(),
            Some("b")
        );
        assert!(journal.exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join(CURSOR)).unwrap(),
            before
        );
        assert_eq!(
            select(root.path(), &["a", "b", "c"], false)
                .unwrap()
                .as_deref(),
            Some("c")
        );
        assert!(!journal.exists());
    }

    #[test]
    fn concurrent_collectors_reserve_distinct_starts_without_lost_updates() {
        let root = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let root = root.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve(&root, &["a".into(), "b".into()].into(), false, true, || {
                        Ok(())
                    })
                    .unwrap()
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let selected = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(selected, ["a".into(), "b".into()].into());
        assert_eq!(
            select(root.path(), &["a", "b"], true).unwrap().as_deref(),
            Some("a")
        );
    }

    #[test]
    fn contention_refuses_without_wait_or_cursor_publication() {
        let root = tempfile::tempdir().unwrap();
        let _held = lk_vault::MutationSession::acquire(root.path()).unwrap();
        let error = select(root.path(), &["a", "b"], false).err().unwrap();
        assert!(error.to_string().contains("busy"));
        assert!(!root.path().join(CURSOR).exists());
    }
}
