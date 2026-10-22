//! A separate byte-preserving store, not the authored Markdown review archive.
use crate::{MutationSession, VaultError};
use lk_core::manual_original::{
    ManualBundleCapture, ManualBundleManifest, ManualBundleMember, ManualOriginalReference,
    ORIGINAL_STORE, OriginalDigest, UnsupportedOriginalConstruct,
};
use serde::Serialize;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

fn error(e: impl std::fmt::Display) -> VaultError {
    VaultError::Original(e.to_string())
}

fn original_path(root: &Path, relative: &Path, create: bool) -> Result<PathBuf, VaultError> {
    crate::original_io::store_path(root, relative, ORIGINAL_STORE, create)
}
use crate::original_io::read_bytes;
fn publish(root: &Path, path: &str, bytes: &[u8]) -> Result<(), VaultError> {
    let full = original_path(root, Path::new(path), true)?;
    match lk_core::fs::publish_exclusive(&full, bytes, Some(0o600))? {
        lk_core::fs::PublishOutcome::Created | lk_core::fs::PublishOutcome::Reused => Ok(()),
        lk_core::fs::PublishOutcome::Occupied => Err(error(format!(
            "immutable original address occupied: {path}"
        ))),
    }
}
fn publish_member(
    root: &Path,
    path: &str,
    bytes: &lk_core::captured_file::CapturedFile,
) -> Result<(), VaultError> {
    // One byte object serves every immutable bundle path through exclusive hard links.
    // There is no full-store search, overwriting rename or cross-filesystem copy fallback.
    let digest = bytes.revision();
    let object = format!("{ORIGINAL_STORE}/objects/{}", digest.value);
    let full = original_path(root, Path::new(&object), true)?;
    match lk_core::fs::publish_exclusive_from(&full, bytes.reader()?, Some(0o600))? {
        lk_core::fs::PublishOutcome::Created | lk_core::fs::PublishOutcome::Reused => {}
        lk_core::fs::PublishOutcome::Occupied => {
            return Err(error(format!(
                "immutable original address occupied: {object}"
            )));
        }
    }
    let source = original_path(root, Path::new(&object), false)?;
    let destination = original_path(root, Path::new(path), true)?;
    match std::fs::hard_link(&source, &destination) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !destination.symlink_metadata()?.file_type().is_file()
                || lk_core::captured_file::hash_reader(
                    std::fs::File::open(&destination)?.take(bytes.byte_len() as u64 + 1),
                )? != (bytes.byte_len(), digest.clone())
            {
                return Err(crate::VaultError::Original(format!(
                    "immutable bundle member occupied: {path}"
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }
    std::fs::File::open(&destination)?.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(
        destination
            .parent()
            .ok_or_else(|| error("original parent"))?,
    )?
    .sync_all()?;
    Ok(())
}

pub fn publish_manual_original(
    session: &MutationSession,
    capture: &ManualBundleCapture,
) -> Result<ManualOriginalReference, VaultError> {
    capture.validate().map_err(error)?;
    let reference = capture.manifest.reference().map_err(error)?;
    let base = reference
        .manifest_path
        .rsplit_once('/')
        .ok_or_else(|| error("manifest address"))?
        .0;
    for (member, bytes) in capture.manifest.members.iter().zip(&capture.contents) {
        publish_member(
            session.root(),
            &format!("{base}/files/{}", member.path),
            bytes,
        )?;
    }
    // The manifest is the commit marker, published only after every member is durable.
    publish(
        session.root(),
        &reference.manifest_path,
        &capture.manifest.bytes().map_err(error)?,
    )?;
    Ok(reference)
}
fn load_manifest(root: &Path, path: &str) -> Result<ManualBundleManifest, VaultError> {
    let full = original_path(root, Path::new(path), false)?;
    let bytes = read_bytes(&full, lk_core::manual_original::MAX_ORIGINAL_MANIFEST_BYTES)?;
    let manifest: ManualBundleManifest = serde_json::from_slice(&bytes).map_err(error)?;
    manifest.validate().map_err(error)?;
    let reference = manifest.reference().map_err(error)?;
    if reference.manifest_path != path || manifest.bytes().map_err(error)? != bytes {
        return Err(error("manifest digest/address/serialization mismatch"));
    }
    Ok(manifest)
}

/// Validate a published bundle before reusing its acquisition receipt. Unlike a
/// paginated original inventory, this verifies every preserved member's bytes.
pub fn load_manual_original_manifest(
    root: &Path,
    reference: &ManualOriginalReference,
) -> Result<ManualBundleManifest, VaultError> {
    reference.validate().map_err(error)?;
    let manifest = load_manifest(root, &reference.manifest_path)?;
    if manifest.reference().map_err(error)? != *reference {
        return Err(error("receipt reference differs from original manifest"));
    }
    for member in &manifest.members {
        load_member(root, reference, &manifest, &member.path)?;
    }
    Ok(manifest)
}
fn load_member(
    root: &Path,
    reference: &ManualOriginalReference,
    manifest: &ManualBundleManifest,
    member: &str,
) -> Result<std::fs::File, VaultError> {
    lk_core::manual_original::validate_member_path(member).map_err(error)?;
    let recorded = manifest
        .members
        .iter()
        .find(|m| m.path == member)
        .ok_or_else(|| error("unknown original member"))?;
    let base = reference
        .manifest_path
        .rsplit_once('/')
        .ok_or_else(|| error("manifest address"))?
        .0;
    let full = original_path(root, Path::new(&format!("{base}/files/{member}")), false)?;
    let mut file = std::fs::File::open(&full)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() != recorded.bytes as u64 {
        return Err(error("original member digest/length mismatch"));
    }
    let (bytes, digest) =
        lk_core::captured_file::hash_reader((&mut file).take(recorded.bytes as u64 + 1))?;
    if bytes != recorded.bytes || digest != recorded.revision {
        return Err(error("original member digest/length mismatch"));
    }
    file.rewind()?;
    Ok(file)
}

fn text_preview(mut file: std::fs::File) -> Result<(Option<String>, bool), VaultError> {
    let mut prefix = Vec::new();
    let mut remainder = Vec::new();
    let mut total = 0usize;
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count;
        prefix
            .extend_from_slice(&buffer[..count.min((65536usize + 4).saturating_sub(prefix.len()))]);
        remainder.extend_from_slice(&buffer[..count]);
        match std::str::from_utf8(&remainder) {
            Ok(_) => remainder.clear(),
            Err(error) if error.error_len().is_none() => {
                remainder.drain(..error.valid_up_to());
            }
            Err(_) => return Ok((None, false)),
        }
    }
    if !remainder.is_empty() {
        return Ok((None, false));
    }
    let valid = match std::str::from_utf8(&prefix) {
        Ok(text) => text,
        Err(failure) => std::str::from_utf8(&prefix[..failure.valid_up_to()]).map_err(error)?,
    };
    let mut end = valid.len().min(65536);
    while !valid.is_char_boundary(end) {
        end -= 1;
    }
    Ok((Some(valid[..end].into()), end < total))
}

pub struct OriginalOptions<'a> {
    pub member: Option<&'a str>,
    pub offset: usize,
    pub limit: usize,
    pub revision: Option<&'a str>,
}
impl Default for OriginalOptions<'_> {
    fn default() -> Self {
        Self {
            member: None,
            offset: 0,
            limit: 20,
            revision: None,
        }
    }
}
#[derive(Serialize)]
pub struct OriginalView {
    pub page: String,
    pub page_revision: String,
    pub available: bool,
    pub reference: Option<ManualOriginalReference>,
    pub unsupported: Vec<UnsupportedOriginalConstruct>,
    pub unsupported_total: usize,
    pub unsupported_truncated: bool,
    pub total_members: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub members: Vec<ManualBundleMember>,
    pub selected_path: Option<String>,
    pub selected_file: Option<PathBuf>,
    pub preview: Option<String>,
    pub preview_truncated: bool,
}
pub fn inspect_original(
    root: &Path,
    page: &Path,
    options: &OriginalOptions<'_>,
) -> Result<OriginalView, VaultError> {
    if options.limit == 0 || options.limit > 100 {
        return Err(error("original limit must be 1..=100"));
    }
    let text = std::fs::read_to_string(crate::page_path(root, page)?)?;
    let revision = blake3::hash(text.as_bytes()).to_hex().to_string();
    let parsed = crate::parse_page(&text).map_err(error)?;
    let value = parsed
        .frontmatter
        .get("source_record")
        .and_then(|r| r.get(lk_core::manual_original::MANUAL_ORIGINAL_FIELD));
    let mut view = OriginalView {
        page: page.to_string_lossy().into(),
        page_revision: revision,
        available: false,
        reference: None,
        unsupported: Vec::new(),
        unsupported_total: 0,
        unsupported_truncated: false,
        total_members: 0,
        offset: options.offset,
        limit: options.limit,
        next_offset: None,
        members: Vec::new(),
        selected_path: None,
        selected_file: None,
        preview: None,
        preview_truncated: false,
    };
    let Some(value) = value else {
        if options.revision.is_some() || options.member.is_some() {
            return Err(error("original unavailable for this page"));
        }
        return Ok(view);
    };
    let reference: ManualOriginalReference =
        serde_json::from_value(value.clone()).map_err(error)?;
    reference.validate().map_err(error)?;
    if options
        .revision
        .is_some_and(|r| r != reference.manifest_revision.value)
    {
        return Err(error("original manifest revision changed"));
    }
    let manifest = load_manifest(root, &reference.manifest_path)?;
    if manifest.reference().map_err(error)? != reference {
        return Err(error("page reference differs from manifest"));
    }
    // Verify the actual entry even for a metadata-only query; never fall back to source_file.
    let entry = load_member(root, &reference, &manifest, &manifest.entry)?;
    let member = options.member.unwrap_or(&manifest.entry);
    let file = if member == manifest.entry {
        entry
    } else {
        load_member(root, &reference, &manifest, member)?
    };
    if options.member.is_some() {
        (view.preview, view.preview_truncated) = text_preview(file)?;
    }
    let base = reference
        .manifest_path
        .rsplit_once('/')
        .ok_or_else(|| error("manifest address"))?
        .0;
    view.selected_path = Some(format!("{base}/files/{member}"));
    view.selected_file = Some(original_path(
        root,
        Path::new(
            view.selected_path
                .as_deref()
                .ok_or_else(|| error("original path"))?,
        ),
        false,
    )?);
    view.available = true;
    view.total_members = manifest.members.len();
    view.next_offset = options
        .offset
        .checked_add(options.limit)
        .filter(|next| *next < view.total_members);
    view.members = manifest
        .members
        .into_iter()
        .skip(options.offset)
        .take(options.limit)
        .collect();
    view.unsupported_total = manifest.unsupported.len();
    view.unsupported_truncated = manifest.unsupported.len() > 10;
    view.unsupported = manifest.unsupported.into_iter().take(10).collect();
    view.reference = Some(reference);
    Ok(view)
}

/// Only a manifest-listed, digest-verified exact member can resolve as hidden evidence.
pub fn is_manual_original(root: &Path, path: &Path) -> Result<bool, VaultError> {
    let Ok(rest) = path.strip_prefix(ORIGINAL_STORE) else {
        return Ok(false);
    };
    let parts: Vec<_> = rest.components().collect();
    if parts.len() < 3 || parts[1].as_os_str() != "files" {
        return Ok(false);
    }
    let Some(digest) = parts[0].as_os_str().to_str() else {
        return Ok(false);
    };
    if (OriginalDigest {
        scheme: "blake3-256".into(),
        value: digest.into(),
    })
    .validate("blake3-256")
    .is_err()
    {
        return Ok(false);
    }
    let manifest_path = format!("{ORIGINAL_STORE}/{digest}/manifest.json");
    let manifest = match load_manifest(root, &manifest_path) {
        Err(VaultError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        result => result?,
    };
    let reference = manifest.reference().map_err(error)?;
    let member = parts[2..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if !manifest
        .members
        .iter()
        .any(|recorded| recorded.path == member)
    {
        return Ok(false);
    }
    match load_member(root, &reference, &manifest, &member) {
        Err(VaultError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        result => result.map(|_| true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::manual_original::{ManualBundleMember, ORIGINAL_SCHEME};
    use tempfile::TempDir;

    #[test]
    fn large_file_backed_original_roundtrips_without_current_capture_configuration() {
        let root = TempDir::new().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let bytes = lk_core::manual_original::MAX_ORIGINAL_ENTRY_BYTES + 1;
        let file = lk_core::captured_file::CapturedFile::capture(
            std::io::Read::take(std::io::repeat(255), bytes as u64),
            bytes,
        )
        .unwrap();
        let scratch = file.path().to_owned();
        let captured = ManualBundleCapture {
            source_file: "/removed/inbox/large.pdf".into(),
            manifest: ManualBundleManifest {
                schema: ORIGINAL_SCHEME.into(),
                entry: "large.pdf".into(),
                members: vec![ManualBundleMember {
                    path: "large.pdf".into(),
                    bytes,
                    revision: file.revision().clone(),
                }],
                unsupported: vec![],
            },
            contents: vec![file],
        };
        let reference = publish_manual_original(&session, &captured).unwrap();
        assert_eq!(reference.scheme, ORIGINAL_SCHEME);
        assert_eq!(
            publish_manual_original(&session, &captured).unwrap(),
            reference
        );
        page(root.path(), &reference);
        drop(captured);
        assert!(!scratch.exists());
        let view = inspect_original(
            root.path(),
            Path::new("wiki/documents/n.md"),
            &OriginalOptions {
                member: Some("large.pdf"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(view.available);
        assert_eq!(view.members[0].bytes, bytes);
        assert!(view.preview.is_none());
        assert_eq!(
            view.selected_file.unwrap().metadata().unwrap().len(),
            bytes as u64
        );
    }

    #[test]
    fn legacy_original_manifest_keeps_its_original_digest_and_reader_contract() {
        let root = TempDir::new().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let mut captured = capture();
        captured.manifest.schema = lk_core::manual_original::LEGACY_ORIGINAL_SCHEME.into();
        let reference = publish_manual_original(&session, &captured).unwrap();
        assert_eq!(
            reference.scheme,
            lk_core::manual_original::LEGACY_ORIGINAL_SCHEME
        );
        page(root.path(), &reference);
        let view = inspect_original(
            root.path(),
            Path::new("wiki/documents/n.md"),
            &OriginalOptions::default(),
        )
        .unwrap();
        assert!(view.available);
        assert_eq!(view.reference.unwrap(), reference);
    }

    fn capture() -> ManualBundleCapture {
        let raw =
            b"---\r\ntitle: \"original\"\r\n---\r\n# exact raw\r\n![x](figure.png)\r\n".to_vec();
        let image = vec![0, 255, 1];
        ManualBundleCapture {
            source_file: "/external/inbox/note.md".into(),
            manifest: ManualBundleManifest {
                schema: ORIGINAL_SCHEME.into(),
                entry: "note.md".into(),
                members: vec![
                    ManualBundleMember {
                        path: "figure.png".into(),
                        bytes: image.len(),
                        revision: OriginalDigest::of(&image),
                    },
                    ManualBundleMember {
                        path: "note.md".into(),
                        bytes: raw.len(),
                        revision: OriginalDigest::of(&raw),
                    },
                ],
                unsupported: vec![],
            },
            contents: vec![
                lk_core::captured_file::CapturedFile::capture(image.as_slice(), image.len())
                    .unwrap(),
                lk_core::captured_file::CapturedFile::capture(raw.as_slice(), raw.len()).unwrap(),
            ],
        }
    }
    fn page(root: &Path, reference: &ManualOriginalReference) {
        std::fs::create_dir_all(root.join("wiki/documents")).unwrap();
        std::fs::write(root.join("wiki/documents/n.md"),format!("---\ntype: document\nsource_file: /external/inbox/note.md\nsource_record: {}\n---\nDerived\n",serde_json::json!({"manual_original":reference}))).unwrap();
    }

    #[test]
    fn publication_receipt_manifest_access_verifies_every_member_and_exact_reference() {
        let root = TempDir::new().unwrap();
        let session = MutationSession::acquire(root.path()).unwrap();
        let captured = capture();
        let reference = publish_manual_original(&session, &captured).unwrap();
        assert_eq!(
            load_manual_original_manifest(root.path(), &reference).unwrap(),
            captured.manifest
        );
        let mut other = reference.clone();
        other.entry_revision = OriginalDigest::of(b"another original");
        assert!(load_manual_original_manifest(root.path(), &other).is_err());
        let base = reference.manifest_path.rsplit_once('/').unwrap().0;
        std::fs::write(root.path().join(base).join("files/figure.png"), [0, 255, 2]).unwrap();
        assert!(load_manual_original_manifest(root.path(), &reference).is_err());
    }
    #[test]
    fn originals_roundtrip_before_page_and_survive_mutable_source_absence() {
        let tmp = TempDir::new().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let captured = capture();
        let reference = publish_manual_original(&session, &captured).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join(reference.entry_vault_path().unwrap())).unwrap(),
            captured.contents[1].read_bytes(1024).unwrap()
        );
        assert_eq!(
            publish_manual_original(&session, &captured).unwrap(),
            reference
        );
        page(tmp.path(), &reference);
        let report = inspect_original(
            tmp.path(),
            Path::new("wiki/documents/n.md"),
            &OriginalOptions {
                member: Some("note.md"),
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(report.available);
        assert_eq!(
            report.preview.unwrap().as_bytes(),
            captured.contents[1].read_bytes(1024).unwrap()
        );
        assert_eq!(report.total_members, 2);
        assert_eq!(report.members.len(), 1);
        let binary = inspect_original(
            tmp.path(),
            Path::new("wiki/documents/n.md"),
            &OriginalOptions {
                member: Some("figure.png"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(binary.preview.is_none());
    }
    #[test]
    fn incomplete_member_publication_retries_but_corrupt_objects_never_get_overwritten() {
        let tmp = TempDir::new().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let captured = capture();
        let reference = captured.manifest.reference().unwrap();
        let base = reference.manifest_path.rsplit_once('/').unwrap().0;
        publish(
            tmp.path(),
            &format!("{base}/files/figure.png"),
            &captured.contents[0].read_bytes(1024).unwrap(),
        )
        .unwrap();
        assert!(!tmp.path().join(&reference.manifest_path).exists());
        publish_manual_original(&session, &captured).unwrap();
        page(tmp.path(), &reference);
        std::fs::write(
            tmp.path().join(reference.entry_vault_path().unwrap()),
            "tampered",
        )
        .unwrap();
        assert!(publish_manual_original(&session, &captured).is_err());
        assert!(
            inspect_original(
                tmp.path(),
                Path::new("wiki/documents/n.md"),
                &OriginalOptions::default()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(reference.entry_vault_path().unwrap()))
                .unwrap(),
            "tampered"
        );
    }
    #[test]
    fn original_query_guards_members_manifest_revision_and_legacy_unavailability() {
        let tmp = TempDir::new().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let reference = publish_manual_original(&session, &capture()).unwrap();
        page(tmp.path(), &reference);
        assert!(
            inspect_original(
                tmp.path(),
                Path::new("wiki/documents/n.md"),
                &OriginalOptions {
                    revision: Some("different"),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            inspect_original(
                tmp.path(),
                Path::new("wiki/documents/n.md"),
                &OriginalOptions {
                    member: Some("../escape"),
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(
            is_manual_original(
                tmp.path(),
                Path::new(&reference.entry_vault_path().unwrap())
            )
            .unwrap()
        );
        assert!(
            !is_manual_original(tmp.path(), Path::new(".lorekeeper/evidence/unknown.md")).unwrap()
        );
        std::fs::write(
            tmp.path().join("wiki/documents/old.md"),
            "---\ntype: document\nsource_file: /mutable/gone\n---\nOld\n",
        )
        .unwrap();
        assert!(
            !inspect_original(
                tmp.path(),
                Path::new("wiki/documents/old.md"),
                &OriginalOptions::default()
            )
            .unwrap()
            .available
        );
    }
    #[cfg(unix)]
    #[test]
    fn store_symlinks_cannot_redirect_original_reads_or_publication() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        std::fs::create_dir_all(tmp.path().join(".lorekeeper/evidence")).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join(ORIGINAL_STORE)).unwrap();
        assert!(publish_manual_original(&session, &capture()).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[cfg(test)]
mod reuse_tests {
    use super::*;
    #[test]
    fn only_changed_member_bytes_add_an_object_across_bundle_revisions() {
        let tmp = tempfile::tempdir().unwrap();
        let session = MutationSession::acquire(tmp.path()).unwrap();
        let build = |image: &[u8]| ManualBundleCapture {
            source_file: "/inbox/n.md".into(),
            manifest: ManualBundleManifest {
                schema: lk_core::manual_original::ORIGINAL_SCHEME.into(),
                entry: "n.md".into(),
                members: vec![
                    ManualBundleMember {
                        path: "a.png".into(),
                        bytes: image.len(),
                        revision: OriginalDigest::of(image),
                    },
                    ManualBundleMember {
                        path: "n.md".into(),
                        bytes: 4,
                        revision: OriginalDigest::of(b"same"),
                    },
                ],
                unsupported: vec![],
            },
            contents: vec![
                lk_core::captured_file::CapturedFile::capture(image, image.len()).unwrap(),
                lk_core::captured_file::CapturedFile::capture(&b"same"[..], 4).unwrap(),
            ],
        };
        let a = publish_manual_original(&session, &build(b"image1")).unwrap();
        let b = publish_manual_original(&session, &build(b"image2")).unwrap();
        assert_ne!(a.manifest_revision, b.manifest_revision);
        assert!(
            same_file::is_same_file(
                tmp.path().join(a.entry_vault_path().unwrap()),
                tmp.path().join(b.entry_vault_path().unwrap())
            )
            .unwrap()
        );
        assert_eq!(
            std::fs::read_dir(tmp.path().join(format!("{ORIGINAL_STORE}/objects")))
                .unwrap()
                .count(),
            3
        );
        let object = tmp.path().join(format!(
            "{ORIGINAL_STORE}/objects/{}",
            OriginalDigest::of(b"same").value
        ));
        std::fs::write(&object, b"bad!").unwrap();
        assert!(publish_manual_original(&session, &build(b"image3")).is_err());
    }
}
