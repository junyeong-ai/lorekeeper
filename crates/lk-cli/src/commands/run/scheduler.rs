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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TurnBinding {
    pub target: String,
    pub identity: String,
}

struct AdmittedTurn {
    binding: TurnBinding,
    remaining: usize,
}

pub(super) struct AdmittedTurns {
    max_consecutive_calls: usize,
    turns: Vec<AdmittedTurn>,
}

impl AdmittedTurns {
    pub fn new(max_consecutive_calls: usize) -> Self {
        Self {
            max_consecutive_calls,
            turns: Vec::new(),
        }
    }

    /// Admit exact bindings only after their ordered fair turns have been reserved.
    pub fn admit(&mut self, bindings: &[TurnBinding]) -> miette::Result<()> {
        if self.max_consecutive_calls == 0 || bindings.is_empty() {
            return Err(miette::miette!(
                "turn admission requires a positive call allowance and reserved bindings"
            ));
        }
        let mut targets = self
            .turns
            .iter()
            .map(|turn| turn.binding.target.as_str())
            .collect::<BTreeSet<_>>();
        for binding in bindings {
            if binding.target.is_empty()
                || binding.identity.is_empty()
                || !targets.insert(&binding.target)
            {
                return Err(miette::miette!(
                    "turn admission requires distinct targets and exact phase identities"
                ));
            }
        }
        self.turns
            .extend(bindings.iter().cloned().map(|binding| AdmittedTurn {
                binding,
                remaining: self.max_consecutive_calls,
            }));
        Ok(())
    }

    pub fn remaining(&self, binding: &TurnBinding) -> usize {
        self.turns
            .iter()
            .find(|turn| turn.binding == *binding)
            .map_or(0, |turn| turn.remaining)
    }

    pub fn remaining_bindings(&self) -> impl Iterator<Item = &TurnBinding> {
        self.turns
            .iter()
            .filter(|turn| turn.remaining != 0)
            .map(|turn| &turn.binding)
    }

    pub fn can_continue(&self, bindings: &[TurnBinding]) -> bool {
        self.charges(bindings).is_ok()
    }

    /// Charge the actual invocation attempts, including repeated bindings in a wave.
    pub fn charge(&mut self, bindings: &[TurnBinding]) -> miette::Result<()> {
        let charges = self.charges(bindings)?;
        for (index, count) in charges {
            self.turns[index].remaining -= count;
        }
        Ok(())
    }

    fn charges(&self, bindings: &[TurnBinding]) -> miette::Result<Vec<(usize, usize)>> {
        if bindings.is_empty() {
            return Err(miette::miette!(
                "a turn continuation requires invocation bindings"
            ));
        }
        let mut charges = std::collections::BTreeMap::new();
        for binding in bindings {
            let index = self
                .turns
                .iter()
                .position(|turn| turn.binding == *binding)
                .ok_or_else(|| {
                    miette::miette!("turn continuation does not match an admitted target and phase")
                })?;
            let count = charges.entry(index).or_insert(0usize);
            *count += 1;
            if *count > self.turns[index].remaining {
                return Err(miette::miette!(
                    "turn continuation exceeds its remaining call allowance"
                ));
            }
        }
        Ok(charges.into_iter().collect())
    }

    pub fn clear_binding(&mut self, binding: &TurnBinding) {
        self.turns.retain(|turn| turn.binding != *binding);
    }

    pub fn clear(&mut self) {
        self.turns.clear();
    }
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

    fn binding(target: &str, identity: &str) -> TurnBinding {
        TurnBinding {
            target: target.into(),
            identity: identity.into(),
        }
    }

    #[test]
    fn admitted_turns_charge_attempts_only_and_require_exact_target_and_phase() {
        let selected = binding("a", "evidence:work-1");
        let mut turns = AdmittedTurns::new(2);
        assert!(!turns.can_continue(std::slice::from_ref(&selected)));
        turns.admit(std::slice::from_ref(&selected)).unwrap();
        assert_eq!(
            turns.remaining(&selected),
            2,
            "admission is not a host attempt"
        );
        for changed in [
            binding("a", "evidence:work-2"),
            binding("a", "screening:work-1"),
            binding("b", "evidence:work-1"),
        ] {
            assert_eq!(turns.remaining(&changed), 0);
            assert!(!turns.can_continue(std::slice::from_ref(&changed)));
            assert!(turns.charge(&[changed]).is_err());
            assert_eq!(turns.remaining(&selected), 2);
        }
        for remaining in [1, 0] {
            assert!(turns.can_continue(std::slice::from_ref(&selected)));
            turns.charge(std::slice::from_ref(&selected)).unwrap();
            assert_eq!(turns.remaining(&selected), remaining);
        }
        assert!(!turns.can_continue(std::slice::from_ref(&selected)));
        assert!(turns.charge(std::slice::from_ref(&selected)).is_err());
        assert!(turns.remaining_bindings().next().is_none());
    }

    #[test]
    fn admitted_wave_preserves_first_reservation_order_and_counts_repeated_work() {
        let z = binding("z", "evidence:z");
        let a = binding("a", "source:a");
        let m = binding("m", "evidence:m");
        let mut turns = AdmittedTurns::new(3);
        turns.admit(&[z.clone(), a.clone()]).unwrap();
        turns.admit(std::slice::from_ref(&m)).unwrap();
        assert_eq!(
            turns.remaining_bindings().cloned().collect::<Vec<_>>(),
            [z.clone(), a.clone(), m.clone()]
        );
        let wave = [a.clone(), z.clone(), a.clone()];
        assert!(turns.can_continue(&wave));
        turns.charge(&wave).unwrap();
        assert_eq!(turns.remaining(&z), 2);
        assert_eq!(turns.remaining(&a), 1);
        assert_eq!(turns.remaining(&m), 3);
        turns.charge(&[z.clone(), z]).unwrap();
        assert_eq!(
            turns.remaining_bindings().cloned().collect::<Vec<_>>(),
            [a, m]
        );
    }

    #[test]
    fn refused_wave_and_duplicate_admission_never_partially_consume_or_reset_turns() {
        let a = binding("a", "evidence:a");
        let b = binding("b", "evidence:b");
        let mut turns = AdmittedTurns::new(2);
        turns.admit(std::slice::from_ref(&a)).unwrap();
        assert!(turns.admit(&[b.clone(), b.clone()]).is_err());
        assert_eq!(turns.remaining(&b), 0);
        assert!(turns.admit(&[b.clone(), a.clone()]).is_err());
        assert_eq!(turns.remaining(&b), 0);
        turns.charge(std::slice::from_ref(&a)).unwrap();
        assert!(turns.admit(std::slice::from_ref(&a)).is_err());
        assert!(turns.admit(&[binding("a", "another-phase")]).is_err());
        assert_eq!(turns.remaining(&a), 1);
        turns.admit(std::slice::from_ref(&b)).unwrap();
        for wave in [
            vec![b.clone(), a.clone(), a.clone()],
            vec![b.clone(), binding("a", "wrong-phase")],
        ] {
            assert!(!turns.can_continue(&wave));
            assert!(turns.charge(&wave).is_err());
            assert_eq!(turns.remaining(&a), 1);
            assert_eq!(turns.remaining(&b), 2);
        }
        assert_eq!(
            turns.remaining_bindings().cloned().collect::<Vec<_>>(),
            [a, b]
        );
    }

    #[test]
    fn clearing_requires_exact_binding_and_new_runtime_has_no_continuation() {
        let a = binding("a", "evidence:a");
        let b = binding("b", "evidence:b");
        let mut turns = AdmittedTurns::new(2);
        turns.admit(&[a.clone(), b.clone()]).unwrap();
        turns.charge(std::slice::from_ref(&b)).unwrap();
        turns.clear_binding(&binding("a", "another-phase"));
        assert_eq!(turns.remaining(&a), 2);
        turns.clear_binding(&a);
        assert_eq!(turns.remaining(&a), 0);
        assert_eq!(turns.remaining(&b), 1);
        assert_eq!(turns.remaining_bindings().collect::<Vec<_>>(), [&b]);
        let restarted = AdmittedTurns::new(2);
        assert!(!restarted.can_continue(std::slice::from_ref(&b)));
        turns.clear();
        assert!(turns.remaining_bindings().next().is_none());
        assert!(!turns.can_continue(std::slice::from_ref(&b)));
    }

    #[test]
    fn empty_admission_zero_allowance_and_empty_attempts_refuse() {
        let a = binding("a", "evidence:a");
        assert!(
            AdmittedTurns::new(0)
                .admit(std::slice::from_ref(&a))
                .is_err()
        );
        let mut turns = AdmittedTurns::new(2);
        for invalid in [
            Vec::new(),
            vec![binding("", "identity")],
            vec![binding("a", "")],
        ] {
            assert!(turns.admit(&invalid).is_err());
        }
        turns.admit(std::slice::from_ref(&a)).unwrap();
        assert!(!turns.can_continue(&[]));
        assert!(turns.charge(&[]).is_err());
        assert_eq!(turns.remaining(&a), 2);
    }

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
