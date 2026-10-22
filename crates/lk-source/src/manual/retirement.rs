//! Optional inbox retirement after the caller verifies canonical publication.
use std::collections::BTreeSet;
use std::path::Path;

use lk_core::captured_file::CapturedFile;
use lk_core::manual_original::{ManualBundleCapture, ManualBundleManifest};

use super::ManualDependencyInventory;
use crate::{ExtractContext, SourceError};

fn error(message: impl std::fmt::Display) -> SourceError {
    SourceError::Parse(format!("manual retirement: {message}"))
}

/// The caller verifies publication, page ownership, processing policy, and immutable originals.
/// Any source capture or publication failure must retain that source's inputs.
/// This helper proves only current local bytes and incoming-reference coverage before archival.
/// Receipt-only entries supply their verified manifest without fabricating new events.
pub fn archive_verified_manual_originals(
    params: &serde_json::Value,
    manifests: &[ManualBundleManifest],
    captures: &[ManualBundleCapture],
    inventory: &ManualDependencyInventory,
    ctx: &ExtractContext,
) -> Result<(), SourceError> {
    let params: super::ManualParams = crate::parse_validated(params)?;
    if !params.archive_after_ingest || manifests.is_empty() {
        return Ok(());
    }
    let inbox = super::resolve_inbox_dir(&params.inbox_dir, &ctx.vault_root, &ctx.vault_dirs)?;
    let root = inbox.canonicalize().map_err(error)?;
    if !inventory.is_for_root(&root) {
        return Err(error("dependency inventory belongs to another inbox"));
    }
    if !inventory.diagnostics().is_empty() {
        tracing::warn!("manual: dependency coverage is incomplete; retaining inbox originals");
        return Ok(());
    }
    let mut entries = BTreeSet::new();
    for manifest in manifests {
        manifest.validate().map_err(error)?;
        if Path::new(&manifest.entry).components().count() != 1 {
            return Err(error("only direct inbox entries may retire"));
        }
        if !inventory.has_entry(&manifest.entry) {
            return Err(error(
                "retirement entry is absent from the exact inbox inventory",
            ));
        }
        if !entries.insert(manifest.entry.as_str()) {
            return Err(error("retirement candidates require distinct entries"));
        }
    }
    let archive = root.join("archived").join(ctx.target_date.to_string());
    let archive_real = lk_core::fs::canonical_prefix(&archive);
    if archive_real == root || !archive_real.starts_with(&root) {
        return Err(SourceError::InvalidParams(
            "manual archive directory must remain within its inbox".into(),
        ));
    }
    let mut started = false;
    for manifest in manifests {
        if inventory.is_referenced(&manifest.entry) || !manifest.unsupported.is_empty() {
            continue;
        }
        if !super::original_current(&root, manifest)? {
            tracing::warn!(
                entry = manifest.entry,
                "manual: bundle changed; retaining inbox original"
            );
            continue;
        }
        let entry = manifest
            .members
            .iter()
            .find(|member| member.path == manifest.entry)
            .expect("validated manifest contains its entry");
        let source = crate::manual_bundle::direct_member(&root, Path::new(&entry.path))?;
        let borrowed = captures
            .iter()
            .find(|capture| capture.manifest == *manifest)
            .and_then(|capture| {
                capture
                    .manifest
                    .members
                    .iter()
                    .position(|member| member.path == manifest.entry)
                    .and_then(|index| capture.contents.get(index))
            });
        let owned = if borrowed.is_none() {
            let file = std::fs::File::open(&source).map_err(error)?;
            Some(CapturedFile::capture(file, entry.bytes).map_err(error)?)
        } else {
            None
        };
        let captured = borrowed.or(owned.as_ref()).expect("captured entry");
        if captured.byte_len() != entry.bytes
            || captured.revision() != &entry.revision
            || !super::original_unchanged(&source, captured)
        {
            tracing::warn!(
                entry = manifest.entry,
                "manual: entry changed; retaining inbox original"
            );
            continue;
        }
        if !started {
            // This is one batch proof immediately before our first owned mutation. Creating
            // `archived` and removing entries intentionally change its membership afterward.
            // External editors are outside this cooperative snapshot; each deletion still
            // rechecks the complete candidate bundle after durable archive publication.
            if !inventory.is_current()? {
                tracing::warn!("manual: dependency inventory changed; retaining inbox originals");
                return Ok(());
            }
            std::fs::create_dir_all(&archive).map_err(error)?;
            started = true;
        }
        if let Err(failure) =
            super::preserve_captured_archive(&archive, Path::new(&entry.path).as_os_str(), captured)
        {
            tracing::warn!(entry = manifest.entry, error = %failure, "manual: archive failed; retaining inbox original");
            continue;
        }
        if super::original_current(&root, manifest)? {
            if let Err(failure) = std::fs::remove_file(&source) {
                tracing::warn!(entry = manifest.entry, error = %failure, "manual: failed to remove archived inbox original");
            }
        } else {
            tracing::warn!(
                entry = manifest.entry,
                "manual: bundle changed during archival; retaining inbox original"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn context(root: PathBuf) -> ExtractContext {
        ExtractContext {
            target_date: jiff::civil::date(2026, 5, 24),
            timezone: jiff::tz::TimeZone::UTC,
            locale: Default::default(),
            identity: Default::default(),
            vault_root: root,
            vault_dirs: Default::default(),
            documents: None,
            projection_cache: None,
            originals: Default::default(),
            max_input_bytes: lk_core::config::IngestInputLimits::default()
                .max_source_bytes
                .get(),
        }
    }

    fn fixture() -> (
        tempfile::TempDir,
        PathBuf,
        ExtractContext,
        serde_json::Value,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("inbox");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let context = context(directory.path().to_owned());
        (
            directory,
            root,
            context,
            serde_json::json!({"inbox_dir":"inbox"}),
        )
    }

    fn capture(root: &Path, name: &str, text: &str) -> ManualBundleCapture {
        std::fs::write(root.join(name), text).unwrap();
        crate::manual_bundle::capture(root, &root.join(name)).unwrap()
    }

    #[test]
    fn verified_receipt_only_entries_and_current_captures_retire_in_one_batch() {
        let (_directory, root, context, params) = fixture();
        let first = capture(&root, "first.md", "First source.");
        let second = capture(&root, "second.txt", "Second source.");
        let manifests = vec![first.manifest.clone(), second.manifest.clone()];
        drop(first);
        let inventory = ManualDependencyInventory::capture(&root, 4096).unwrap();
        archive_verified_manual_originals(&params, &manifests, &[second], &inventory, &context)
            .unwrap();
        for name in ["first.md", "second.txt"] {
            assert!(!root.join(name).exists());
            assert!(root.join("archived/2026-05-24").join(name).is_file());
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn omitted_incoming_owners_protect_receipt_only_entries_and_assets_stay() {
        let (_directory, root, context, params) = fixture();
        let target = capture(&root, "target.txt", "Published target.");
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/image.png"), b"attachment").unwrap();
        let owner = capture(
            &root,
            "owner.md",
            "[target](target.txt)\n![asset](assets/image.png)",
        );
        let manifests = vec![target.manifest.clone(), owner.manifest.clone()];
        drop(target);
        let inventory = ManualDependencyInventory::capture(&root, 4096).unwrap();
        archive_verified_manual_originals(&params, &manifests, &[owner], &inventory, &context)
            .unwrap();
        assert!(root.join("target.txt").is_file());
        assert!(!root.join("owner.md").exists());
        assert_eq!(
            std::fs::read(root.join("assets/image.png")).unwrap(),
            b"attachment"
        );
    }

    #[test]
    fn changed_or_unknown_inventory_never_starts_archival() {
        for unknown in [false, true] {
            let (_directory, root, context, params) = fixture();
            let original = capture(&root, "entry.txt", "Published source.");
            if unknown {
                std::fs::write(root.join("unknown.css"), "@import 'entry.txt';").unwrap();
            }
            let inventory = ManualDependencyInventory::capture(&root, 4096).unwrap();
            if !unknown {
                std::fs::write(root.join("later.md"), "[new owner](entry.txt)").unwrap();
            }
            archive_verified_manual_originals(
                &params,
                std::slice::from_ref(&original.manifest),
                std::slice::from_ref(&original),
                &inventory,
                &context,
            )
            .unwrap();
            assert!(root.join("entry.txt").is_file());
            assert!(!root.join("archived").exists());
        }
    }

    #[test]
    fn changed_members_remain_but_verified_retirement_uses_historical_manifest_bounds() {
        let (_directory, root, mut context, params) = fixture();
        std::fs::write(root.join("asset.txt"), "Old asset.").unwrap();
        let original = capture(&root, "entry.md", "[asset](asset.txt)");
        let manifest = original.manifest.clone();
        drop(original);
        std::fs::write(root.join("asset.txt"), "New asset.").unwrap();
        let inventory = ManualDependencyInventory::capture(&root, 4096).unwrap();
        archive_verified_manual_originals(&params, &[manifest], &[], &inventory, &context).unwrap();
        assert!(root.join("entry.md").is_file());
        assert!(!root.join("archived").exists());

        let original = capture(&root, "binary.pdf", "previously published original");
        let manifest = original.manifest.clone();
        drop(original);
        context.originals.max_entry_bytes = std::num::NonZeroUsize::new(1).unwrap();
        let inventory = ManualDependencyInventory::capture(&root, 4096).unwrap();
        archive_verified_manual_originals(&params, &[manifest], &[], &inventory, &context).unwrap();
        assert!(!root.join("binary.pdf").exists());
        assert!(root.join("archived").exists());
    }

    #[test]
    fn disabled_retirement_and_foreign_inventory_do_not_modify_inputs() {
        let (_directory, root, context, params) = fixture();
        let original = capture(&root, "entry.txt", "Published source.");
        let other = tempfile::tempdir().unwrap();
        let inventory = ManualDependencyInventory::capture(other.path(), 4096).unwrap();
        assert!(
            archive_verified_manual_originals(
                &params,
                std::slice::from_ref(&original.manifest),
                &[],
                &inventory,
                &context,
            )
            .is_err()
        );
        let params = serde_json::json!({"inbox_dir":"inbox", "archive_after_ingest":false});
        archive_verified_manual_originals(
            &params,
            std::slice::from_ref(&original.manifest),
            std::slice::from_ref(&original),
            &inventory,
            &context,
        )
        .unwrap();
        assert!(root.join("entry.txt").is_file());
        assert!(!root.join("archived").exists());
    }

    #[test]
    fn candidate_spelling_requires_the_exact_inventoried_entry() {
        let (_directory, root, context, params) = fixture();
        let original = capture(&root, "entry.txt", "Published source.");
        let mut manifest = original.manifest.clone();
        manifest.entry = "ENTRY.txt".into();
        manifest.members[0].path = "ENTRY.txt".into();
        let inventory = ManualDependencyInventory::capture(&root, 4096).unwrap();
        assert!(
            archive_verified_manual_originals(&params, &[manifest], &[], &inventory, &context)
                .is_err()
        );
        assert!(root.join("entry.txt").is_file());
        assert!(!root.join("archived").exists());
    }
}
