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

#[cfg(test)]
mod tests {
    use super::*;

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
