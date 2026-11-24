use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(super) struct AuditReplans {
    spent: BTreeSet<String>,
    groups: BTreeMap<String, BTreeSet<String>>,
}

pub(super) struct Slot(BTreeSet<String>);

impl AuditReplans {
    pub(super) fn bind(&mut self, path: &str) -> Slot {
        Slot(
            self.groups
                .entry(path.to_owned())
                .or_insert_with(|| BTreeSet::from([path.to_owned()]))
                .clone(),
        )
    }

    pub(super) fn try_spend(&mut self, slot: &Slot) -> bool {
        let mut allowed = false;
        for original in &slot.0 {
            allowed |= self.spent.insert(original.clone());
        }
        allowed
    }

    // Budget ancestry never resolves a graph subject or authorizes reuse of captured inputs.
    pub(super) fn inherit(&mut self, original: &str, current: &str) {
        let slot = self.bind(original);
        self.groups
            .entry(current.to_owned())
            .or_default()
            .extend(slot.0);
    }
}

/// Queue targets whose finalizer inputs changed under a fresh guard. Each target may replan
/// once per run; a repeated change defers it, so a host that keeps changing its own scope cannot
/// loop through paid calls or fresh plans.
#[derive(Default)]
pub(super) struct FinalizationReplans {
    spent: BTreeSet<String>,
    reviews: BTreeSet<String>,
    deferred: BTreeMap<String, String>,
}

impl FinalizationReplans {
    /// Spends `target`'s replan; once it is spent, defers the target with `reason` instead.
    pub(super) fn try_spend(&mut self, target: &str, reason: &str) -> bool {
        if self.spent.insert(target.to_owned()) {
            return true;
        }
        self.deferred.insert(target.to_owned(), reason.to_owned());
        false
    }

    pub(super) fn deferral(&self, target: &str) -> Option<&str> {
        self.deferred.get(target).map(String::as_str)
    }

    /// Whether finalizing `targets` may publish a review this run.
    pub(super) fn review_allowance(
        &self,
        targets: &[String],
    ) -> crate::commands::work::generation::ReviewAllowance {
        if targets.iter().any(|target| self.reviews.contains(target)) {
            crate::commands::work::generation::ReviewAllowance::Spent
        } else {
            crate::commands::work::generation::ReviewAllowance::Available
        }
    }

    /// Spends `target`'s one review publication for this run.
    pub(super) fn spend_review(&mut self, target: &str) {
        self.reviews.insert(target.to_owned());
    }

    /// Defers `target` for the rest of this run with `reason`.
    pub(super) fn defer(&mut self, target: &str, reason: &str) {
        self.deferred.insert(target.to_owned(), reason.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_finalization_change_defers_only_its_target() {
        let mut replans = FinalizationReplans::default();
        assert!(replans.try_spend("wiki/documents/a.md", "changed again"));
        assert!(replans.deferral("wiki/documents/a.md").is_none());
        assert!(!replans.try_spend("wiki/documents/a.md", "changed again"));
        assert_eq!(
            replans.deferral("wiki/documents/a.md"),
            Some("changed again")
        );
        assert!(replans.try_spend("wiki/documents/b.md", "changed again"));
        assert!(replans.deferral("wiki/documents/b.md").is_none());
    }

    #[test]
    fn reacquisition_does_not_renew_spent_subject_allowances() {
        let mut allowance = AuditReplans::default();
        let first = allowance.bind("wiki/concepts/subject.md");
        assert!(allowance.try_spend(&first));
        let next = allowance.bind("wiki/concepts/subject.md");
        assert!(!allowance.try_spend(&next));
    }

    #[test]
    fn a_proven_survivor_inherits_spent_originals_without_new_allowances() {
        let mut allowance = AuditReplans::default();
        let original = allowance.bind("wiki/concepts/original.md");
        assert!(allowance.try_spend(&original));
        allowance.inherit("wiki/concepts/original.md", "wiki/concepts/survivor.md");
        let next = allowance.bind("wiki/concepts/survivor.md");
        assert!(!allowance.try_spend(&next));
    }

    #[test]
    fn merged_original_slots_keep_unspent_allowances_and_their_existing_bindings() {
        let mut allowance = AuditReplans::default();
        let original = allowance.bind("wiki/concepts/original.md");
        let survivor = allowance.bind("wiki/concepts/survivor.md");
        assert!(allowance.try_spend(&original));
        allowance.inherit("wiki/concepts/original.md", "wiki/concepts/survivor.md");
        assert!(!allowance.try_spend(&original));
        let next = allowance.bind("wiki/concepts/survivor.md");
        assert!(allowance.try_spend(&next));
        assert!(!allowance.try_spend(&survivor));
        let next = allowance.bind("wiki/concepts/survivor.md");
        assert!(!allowance.try_spend(&next));
    }

    #[test]
    fn an_existing_slot_cannot_borrow_a_later_merged_originals_allowance() {
        let mut allowance = AuditReplans::default();
        let survivor = allowance.bind("wiki/concepts/survivor.md");
        allowance.bind("wiki/concepts/original.md");
        assert!(allowance.try_spend(&survivor));
        allowance.inherit("wiki/concepts/original.md", "wiki/concepts/survivor.md");
        assert!(!allowance.try_spend(&survivor));
        let next = allowance.bind("wiki/concepts/survivor.md");
        assert!(allowance.try_spend(&next));
    }
}
