//! The immutable manual original contract; independent of document location ownership.
use std::collections::BTreeSet;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

pub const MANUAL_ORIGINAL_FIELD: &str = "manual_original";
pub const ORIGINAL_SCHEME: &str = "manual-bundle-v1";
pub const ORIGINAL_STORE: &str = ".lorekeeper/evidence/manual";
pub const MAX_MANUAL_BATCH_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_ORIGINAL_ENTRY_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_ORIGINAL_MEMBER_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ORIGINAL_MEMBERS: usize = 2048;
pub const MAX_ORIGINAL_MANIFEST_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OriginalDigest {
    pub scheme: String,
    pub value: String,
}
impl OriginalDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            scheme: "blake3-256".into(),
            value: blake3::hash(bytes).to_hex().to_string(),
        }
    }
    pub fn validate(&self, scheme: &str) -> Result<(), String> {
        if self.scheme != scheme
            || self.value.len() != 64
            || !self
                .value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid original revision scheme/digest".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManualOriginalReference {
    pub scheme: String,
    pub manifest_path: String,
    pub manifest_revision: OriginalDigest,
    pub entry_path: String,
    pub entry_revision: OriginalDigest,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManualBundleMember {
    pub path: String,
    pub bytes: usize,
    pub revision: OriginalDigest,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct UnsupportedOriginalConstruct {
    pub member: String,
    pub construct: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManualBundleManifest {
    pub schema: String,
    pub entry: String,
    pub members: Vec<ManualBundleMember>,
    /// Incomplete dependency discovery is distinct from a complete listed byte inventory.
    pub unsupported: Vec<UnsupportedOriginalConstruct>,
}
/// Captured bytes travel beside RawItem, never as JSON/base64 metadata.
#[derive(Debug)]
pub struct ManualBundleCapture {
    pub source_file: String,
    pub manifest: ManualBundleManifest,
    pub contents: Vec<Vec<u8>>,
}

pub fn validate_member_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("original member requires a normalized relative path".into());
    }
    Ok(())
}
impl ManualBundleManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != ORIGINAL_SCHEME
            || self.members.is_empty()
            || self.members.len() > MAX_ORIGINAL_MEMBERS
        {
            return Err("invalid original manifest schema/member count".into());
        }
        validate_member_path(&self.entry)?;
        let mut paths = BTreeSet::new();
        let mut total = 0usize;
        for member in &self.members {
            validate_member_path(&member.path)?;
            member.revision.validate("blake3-256")?;
            if !paths.insert(&member.path)
                || member.bytes > MAX_ORIGINAL_MEMBER_BYTES
                || (member.path == self.entry && member.bytes > MAX_ORIGINAL_ENTRY_BYTES)
            {
                return Err("duplicate/oversize original member".into());
            }
            total = total
                .checked_add(member.bytes)
                .ok_or("original size overflow")?;
        }
        if total > MAX_MANUAL_BATCH_BYTES
            || !paths.contains(&self.entry)
            || self.members.windows(2).any(|w| w[0].path >= w[1].path)
        {
            return Err("invalid original inventory/order/entry/size".into());
        }
        for issue in &self.unsupported {
            if !paths.contains(&issue.member) {
                return Err("original coverage issue names an unknown member".into());
            }
        }
        if self
            .unsupported
            .iter()
            .any(|issue| issue.construct.is_empty() || issue.construct.len() > 128)
        {
            return Err("invalid original coverage construct".into());
        }
        if self.unsupported.windows(2).any(|w| w[0] >= w[1]) {
            return Err("original coverage must be sorted and unique".into());
        }
        Ok(())
    }
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_ORIGINAL_MANIFEST_BYTES {
            return Err(
                "original manifest exceeds 2 MiB; provide a smaller self-contained export".into(),
            );
        }
        Ok(bytes)
    }
    pub fn revision(&self) -> Result<OriginalDigest, String> {
        let mut hash = blake3::Hasher::new();
        hash.update(b"lore-manual-bundle-v1\0");
        hash.update(&self.bytes()?);
        Ok(OriginalDigest {
            scheme: "manual-bundle-v1-blake3-256".into(),
            value: hash.finalize().to_hex().to_string(),
        })
    }
    pub fn reference(&self) -> Result<ManualOriginalReference, String> {
        let revision = self.revision()?;
        Ok(ManualOriginalReference {
            scheme: ORIGINAL_SCHEME.into(),
            manifest_path: format!("{ORIGINAL_STORE}/{}/manifest.json", revision.value),
            manifest_revision: revision,
            entry_path: format!("files/{}", self.entry),
            entry_revision: self
                .members
                .iter()
                .find(|m| m.path == self.entry)
                .ok_or("missing entry")?
                .revision
                .clone(),
        })
    }
}
impl ManualOriginalReference {
    pub fn validate(&self) -> Result<(), String> {
        self.manifest_revision
            .validate("manual-bundle-v1-blake3-256")?;
        self.entry_revision.validate("blake3-256")?;
        if self.scheme != ORIGINAL_SCHEME
            || self.manifest_path
                != format!(
                    "{ORIGINAL_STORE}/{}/manifest.json",
                    self.manifest_revision.value
                )
            || !self.entry_path.starts_with("files/")
        {
            return Err("invalid original reference address".into());
        }
        validate_member_path(&self.entry_path)
    }
    pub fn entry_vault_path(&self) -> Result<String, String> {
        self.validate()?;
        Ok(format!(
            "{ORIGINAL_STORE}/{}/{}",
            self.manifest_revision.value, self.entry_path
        ))
    }
}
impl ManualBundleCapture {
    pub fn validate(&self) -> Result<(), String> {
        self.manifest.validate()?;
        if self.contents.len() != self.manifest.members.len() {
            return Err("incomplete captured original inventory".into());
        }
        for (member, bytes) in self.manifest.members.iter().zip(&self.contents) {
            if bytes.len() != member.bytes || OriginalDigest::of(bytes) != member.revision {
                return Err("captured original bytes mismatch".into());
            }
        }
        Ok(())
    }
    pub fn byte_len(&self) -> usize {
        self.contents.iter().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> ManualBundleManifest {
        ManualBundleManifest {
            schema: ORIGINAL_SCHEME.into(),
            entry: "note.md".into(),
            members: vec![ManualBundleMember {
                path: "note.md".into(),
                bytes: 3,
                revision: OriginalDigest::of(b"raw"),
            }],
            unsupported: vec![],
        }
    }
    #[test]
    fn original_revisions_bind_member_bytes_paths_and_coverage_not_location() {
        let a = manifest();
        let mut b = a.clone();
        b.members[0].revision = OriginalDigest::of(b"new");
        assert_ne!(a.revision().unwrap(), b.revision().unwrap());
        b = a.clone();
        b.unsupported.push(UnsupportedOriginalConstruct {
            member: "note.md".into(),
            construct: "html-style".into(),
        });
        assert_ne!(a.revision().unwrap(), b.revision().unwrap());
        let reference = a.reference().unwrap();
        assert!(
            reference
                .entry_vault_path()
                .unwrap()
                .ends_with("/files/note.md")
        );
        reference.validate().unwrap();
    }
    #[test]
    fn original_roster_and_reference_refuse_invented_or_escaping_addresses() {
        let mut a = manifest();
        a.entry = "absent.md".into();
        assert!(a.validate().is_err());
        for p in ["../outside", "/absolute", "a/../b", "a\\b", "a//b", "a/./b"] {
            assert!(validate_member_path(p).is_err(), "{p}");
        }
        let mut r = manifest().reference().unwrap();
        r.manifest_path = ".lorekeeper/evidence/another.md".into();
        assert!(r.validate().is_err());
    }
}
