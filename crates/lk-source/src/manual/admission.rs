use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use lk_core::manual_original::{ManualOriginalReference, validate_member_path};

use crate::SourceError;

const VERSION: u8 = 1;

fn invalid(message: impl std::fmt::Display) -> SourceError {
    SourceError::Parse(format!("manual admission: {message}"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualPublishedInput {
    pub policy_revision: String,
    /// Opaque pipeline-owned recipe; acquisition does not interpret render policy.
    pub processing_recipe: serde_json::Value,
    pub original: ManualOriginalReference,
    pub target: String,
}

impl ManualPublishedInput {
    fn validate(&self) -> Result<(), SourceError> {
        validate_revision(&self.policy_revision)?;
        if !self.processing_recipe.is_object()
            || blake3::hash(&serde_json::to_vec(&self.processing_recipe).map_err(invalid)?)
                .to_hex()
                .as_str()
                != self.policy_revision
        {
            return Err(invalid("processing recipe checksum differs"));
        }
        self.original.validate().map_err(invalid)?;
        validate_member_path(&self.target).map_err(invalid)?;
        if Path::new(&self.target)
            .extension()
            .is_none_or(|value| value != "md")
        {
            return Err(invalid("published target must be a Markdown page"));
        }
        Ok(())
    }
}

/// Collection receipts bind published acquisitions, never completed semantic work.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualAdmission {
    version: u8,
    inbox: PathBuf,
    pending: Vec<String>,
    published: BTreeMap<String, ManualPublishedInput>,
}

impl ManualAdmission {
    pub fn new(inbox: PathBuf) -> Self {
        Self {
            version: VERSION,
            inbox,
            pending: Vec::new(),
            published: BTreeMap::new(),
        }
    }

    pub fn decode(raw: &str, inbox: &Path) -> Result<Self, SourceError> {
        let value: serde_json::Value = serde_json::from_str(raw).map_err(invalid)?;
        if !value.is_object()
            || value.get("published").is_none_or(|published| {
                !published.is_object()
                    || published.as_object().is_some_and(|entries| {
                        entries.values().any(|receipt| {
                            !receipt.is_object()
                                || receipt
                                    .get("original")
                                    .is_none_or(|value| !value.is_object())
                        })
                    })
            })
        {
            return Err(invalid("state and receipts require named object fields"));
        }
        let state: Self = serde_json::from_value(value).map_err(invalid)?;
        state.validate(inbox)?;
        Ok(state)
    }

    pub fn validate(&self, inbox: &Path) -> Result<(), SourceError> {
        if self.version != VERSION || self.inbox != inbox || !inbox.is_absolute() {
            return Err(invalid("state version or canonical inbox differs"));
        }
        let mut names = BTreeSet::new();
        for entry in &self.pending {
            validate_entry(entry)?;
            if !names.insert(entry) {
                return Err(invalid("duplicate pending entry"));
            }
        }
        for (entry, receipt) in &self.published {
            validate_entry(entry)?;
            receipt.validate()?;
            if receipt.original.entry_path != format!("files/{entry}") {
                return Err(invalid("published original names another inbox entry"));
            }
        }
        Ok(())
    }

    pub fn published(&self) -> &BTreeMap<String, ManualPublishedInput> {
        &self.published
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn pending(&self) -> &[String] {
        &self.pending
    }

    pub fn inbox(&self) -> &Path {
        &self.inbox
    }

    /// The caller verifies published acquisitions against current bytes and ownership.
    /// Taking a turn rotates work but records no publication or read coverage.
    pub fn reserve(
        &mut self,
        entries: &BTreeSet<String>,
        verified: &BTreeSet<String>,
        limit: usize,
    ) -> Result<Vec<String>, SourceError> {
        if limit == 0 || limit > lk_core::manual_original::MAX_ORIGINAL_MEMBERS {
            return Err(invalid("entry limit must be 1..=2048"));
        }
        self.reconcile(entries, verified)?;
        let turn = self.pending[..self.pending.len().min(limit)].to_vec();
        self.pending.rotate_left(turn.len());
        Ok(turn)
    }

    /// Refresh membership and verified publication coverage without taking another turn.
    pub fn reconcile(
        &mut self,
        entries: &BTreeSet<String>,
        verified: &BTreeSet<String>,
    ) -> Result<(), SourceError> {
        for entry in entries {
            validate_entry(entry)?;
        }
        if !verified.is_subset(entries)
            || verified
                .iter()
                .any(|entry| !self.published.contains_key(entry))
        {
            return Err(invalid(
                "verified entries require existing current receipts",
            ));
        }
        self.pending
            .retain(|entry| entries.contains(entry) && !verified.contains(entry));
        let known = self.pending.iter().cloned().collect::<BTreeSet<_>>();
        self.pending.extend(
            entries
                .difference(verified)
                .filter(|entry| !known.contains(*entry))
                .cloned(),
        );
        self.published.retain(|entry, _| entries.contains(entry));
        Ok(())
    }

    pub fn record(
        &mut self,
        entry: String,
        receipt: ManualPublishedInput,
    ) -> Result<(), SourceError> {
        validate_entry(&entry)?;
        receipt.validate()?;
        if receipt.original.entry_path != format!("files/{entry}") {
            return Err(invalid("publication receipt names another entry"));
        }
        self.pending.retain(|pending| pending != &entry);
        self.published.insert(entry, receipt);
        Ok(())
    }
}

fn validate_entry(entry: &str) -> Result<(), SourceError> {
    validate_member_path(entry).map_err(invalid)?;
    if Path::new(entry).components().count() != 1 {
        return Err(invalid("entry must be a direct inbox file"));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<(), SourceError> {
    if revision.len() != 64
        || !revision
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
    {
        return Err(invalid(
            "policy requires a complete lowercase BLAKE3 revision",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::manual_original::{
        ManualBundleManifest, ManualBundleMember, ORIGINAL_SCHEME, OriginalDigest,
    };

    fn entries(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).into()).collect()
    }

    fn receipt(entry: &str) -> ManualPublishedInput {
        let manifest = ManualBundleManifest {
            schema: ORIGINAL_SCHEME.into(),
            entry: entry.into(),
            members: vec![ManualBundleMember {
                path: entry.into(),
                bytes: 1,
                revision: OriginalDigest::of(b"x"),
            }],
            unsupported: Vec::new(),
        };
        let processing_recipe = serde_json::json!({"protocol": "fixture"});
        ManualPublishedInput {
            policy_revision: blake3::hash(&serde_json::to_vec(&processing_recipe).unwrap())
                .to_hex()
                .to_string(),
            processing_recipe,
            original: manifest.reference().unwrap(),
            target: "wiki/documents/topic.md".into(),
        }
    }

    #[test]
    fn failed_turns_and_new_entries_preserve_old_pending_order_across_restart() {
        let inbox = Path::new("/inbox");
        let mut state = ManualAdmission::new(inbox.into());
        assert_eq!(
            state
                .reserve(&entries(&["b.md", "c.md", "d.md"]), &BTreeSet::new(), 1)
                .unwrap(),
            ["b.md"]
        );
        let mut state =
            ManualAdmission::decode(&serde_json::to_string(&state).unwrap(), inbox).unwrap();
        assert_eq!(
            state
                .reserve(
                    &entries(&["a.md", "b.md", "c.md", "d.md"]),
                    &BTreeSet::new(),
                    2
                )
                .unwrap(),
            ["c.md", "d.md"]
        );
        assert!(state.published().is_empty());
    }

    #[test]
    fn receipts_require_fresh_verification_and_deleted_inputs_release_scheduler_entries() {
        let mut state = ManualAdmission::new("/inbox".into());
        state.record("a.md".into(), receipt("a.md")).unwrap();
        let all = entries(&["a.md", "b.md"]);
        assert_eq!(
            state.reserve(&all, &entries(&["a.md"]), 1).unwrap(),
            ["b.md"]
        );
        assert_eq!(
            state.reserve(&all, &BTreeSet::new(), 2).unwrap(),
            ["b.md", "a.md"]
        );
        assert_eq!(
            state
                .reserve(&entries(&["c.md"]), &BTreeSet::new(), 1)
                .unwrap(),
            ["c.md"]
        );
        assert!(state.published().is_empty());
    }

    #[test]
    fn malformed_foreign_and_positional_state_refuses_instead_of_resetting() {
        let mut state = ManualAdmission::new("/inbox".into());
        state.record("a.md".into(), receipt("a.md")).unwrap();
        let valid = serde_json::to_value(&state).unwrap();
        assert!(ManualAdmission::decode(&valid.to_string(), Path::new("/other")).is_err());
        for field in ["version", "inbox", "pending", "published"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(ManualAdmission::decode(&missing.to_string(), Path::new("/inbox")).is_err());
        }
        for invalid in ["[]", "{}", "null", "not-json"] {
            assert!(ManualAdmission::decode(invalid, Path::new("/inbox")).is_err());
        }
        let mut positional = valid;
        let record = &positional["published"]["a.md"];
        positional["published"]["a.md"] = serde_json::json!([
            record["policy_revision"],
            record["original"],
            record["target"]
        ]);
        assert!(ManualAdmission::decode(&positional.to_string(), Path::new("/inbox")).is_err());
    }
}
