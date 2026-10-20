//! Single-sourced atomic file write.
//!
//! The temp + fsync + rename + dir-fsync pattern lived as divergent copies
//! across the crates (queue, vault, credentials, event log), several of
//! which got the temp name wrong — a deterministic or pid-only temp that two writers
//! targeting the same path share and truncate, corrupting the file. This is the one
//! atomic-write implementation; every writer goes through it so the durability and
//! per-writer-unique-temp guarantees can't drift per call site. `lk_vault::VaultWriter`
//! delegates here from both its sync path and its async (tokio) path (the latter via
//! `spawn_blocking`), so there is no second atomic-write implementation anywhere.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace `path` with complete, durable bytes. Exclusively create a same-directory
/// temporary with the requested Unix mode before writing, then fsync, rename and
/// sync the parent directory. Occupied temporary names are preserved; only this
/// call's created temporary is cleaned up on failure.
///
/// The temporary keeps the destination extension before `.tmp`, preserving suffix
/// sweeps such as `.jsonl.tmp`. Without an explicit mode, creation follows the
/// platform default and process umask.
pub fn write_atomic(path: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    let (temporary, mut file) = create_temporary_file(|| atomic_name(path), mode.unwrap_or(0o666))?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary.0, path)?;
    sync_parent(path)
}

fn atomic_name(path: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if !extension.is_empty() => {
            path.with_extension(format!("{pid}.{sequence}.{extension}.tmp"))
        }
        _ => path.with_extension(format!("{pid}.{sequence}.tmp")),
    }
}

/// Result of preserving a file at an exact address without replacing any entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Created,
    Reused,
    Occupied,
}

/// Publish complete, fsynced bytes only if `path` is absent. A hard link from a
/// same-directory private temporary file makes the final creation exclusive even
/// when another publisher creates the candidate concurrently. Existing regular
/// files with exactly these bytes are reused; directories, symlinks and different
/// bytes are occupied. I/O errors are never interpreted as absence or collision.
///
/// The caller owns candidate naming and may try another address only on
/// `Occupied`. No parent directory is created. The filesystem must support hard
/// links; failure is explicit, never a fallback to overwriting rename.
///
/// Reuse compares a pinned inode, not bytes read through the destination path.
/// This does not freeze external in-place editors or a parent directory that is
/// replaced externally. A successful reuse is an observation, not a lease for
/// subsequently deleting a source; consumers must guard that operation separately.
pub fn publish_exclusive(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
) -> io::Result<PublishOutcome> {
    publish_exclusive_from(path, std::io::Cursor::new(contents), mode)
}

/// Stream a complete source to the same exclusive publication boundary.
pub fn publish_exclusive_from(
    path: &Path,
    mut contents: impl Read,
    mode: Option<u32>,
) -> io::Result<PublishOutcome> {
    let (temporary, mut file) = publication_temp(path)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    io::copy(&mut contents, &mut file)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);

    // A disappearing occupied entry may be retried, but an uncooperative external
    // writer must not keep a caller spinning indefinitely.
    for _ in 0..16 {
        match std::fs::hard_link(&temporary.0, path) {
            Ok(()) => {
                sync_parent(path)?;
                return Ok(PublishOutcome::Created);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match compare_existing(path, &temporary.0)? {
                    Some(true) => {
                        sync_parent(path)?;
                        return Ok(PublishOutcome::Reused);
                    }
                    Some(false) => return Ok(PublishOutcome::Occupied),
                    None => continue,
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "publication destination kept changing",
    ))
}

struct TemporaryFile(PathBuf);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn publication_name(path: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".lore-publish.{}.{sequence}.tmp",
        std::process::id()
    ))
}

fn publication_temp(path: &Path) -> io::Result<(TemporaryFile, std::fs::File)> {
    create_temporary_file(|| publication_name(path), 0o600)
}

fn create_temporary_file(
    mut name: impl FnMut() -> PathBuf,
    mode: u32,
) -> io::Result<(TemporaryFile, std::fs::File)> {
    #[cfg(not(unix))]
    let _ = mode;
    loop {
        let temporary = name();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((TemporaryFile(temporary), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

// None means the destination disappeared during observation. Pinning is separate
// from opening: on Unix a hard link can preserve the symlink itself, while other
// platforms may follow it. Neither case may cause us to read its target bytes.
fn compare_existing(path: &Path, contents: &Path) -> io::Result<Option<bool>> {
    let regular = |path: &Path| match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type().is_file())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    match regular(path)? {
        Some(true) => {}
        other => return Ok(other),
    }
    let pin = loop {
        let pin = publication_name(path);
        match std::fs::hard_link(path, &pin) {
            Ok(()) => break TemporaryFile(pin),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    };
    if regular(&pin.0)? != Some(true) || regular(path)? != Some(true) {
        return Ok(Some(false));
    }
    if !same_publication_file(path, &pin.0)? {
        return Ok(Some(false));
    }
    // The private pin cannot be redirected by a rename of the public candidate.
    let mut file = std::fs::File::open(&pin.0)?;
    let mut contents = std::fs::File::open(contents)?;
    if file.metadata()?.len() != contents.metadata()?.len() {
        return Ok(Some(false));
    }
    let mut buffer = [0; 65536];
    let mut expected = [0; 65536];
    loop {
        let count = contents.read(&mut expected)?;
        if count == 0 {
            if file.read(&mut buffer[..1])? != 0 {
                return Ok(Some(false));
            }
            break;
        }
        match file.read_exact(&mut buffer[..count]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(Some(false)),
            Err(error) => return Err(error),
        }
        if buffer[..count] != expected[..count] {
            return Ok(Some(false));
        }
    }
    if regular(path)? != Some(true) {
        return Ok(Some(false));
    }
    if !same_publication_file(path, &pin.0)? {
        return Ok(Some(false));
    }
    file.sync_all()?;
    Ok(Some(true))
}

fn same_publication_file(path: &Path, pin: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let public = std::fs::symlink_metadata(path)?;
        let pinned = std::fs::symlink_metadata(pin)?;
        Ok(public.file_type().is_file()
            && pinned.file_type().is_file()
            && (public.dev(), public.ino()) == (pinned.dev(), pinned.ino()))
    }
    #[cfg(not(unix))]
    {
        same_file::is_same_file(path, pin)
    }
}

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        std::fs::File::open(parent.unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Canonicalize the longest EXISTING ancestor of `path` (resolving symlinks there) and
/// re-attach the not-yet-existent remainder, so two paths that share a real, possibly
/// symlinked ancestor are compared on the same canonical basis even when neither fully
/// exists. `Path::canonicalize` requires the whole path to exist; this degrades to that
/// when it does, and to a coherent partial resolution when it doesn't — never mixing a
/// resolved side with a lexical one. Falls back to the lexical path only when nothing
/// in the chain exists (e.g. an empty or fully-synthetic path).
pub fn canonical_prefix(path: &Path) -> std::path::PathBuf {
    let mut ancestor = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = ancestor.canonicalize() {
            let mut resolved = canon;
            for segment in tail.iter().rev() {
                resolved.push(segment);
            }
            return resolved;
        }
        let Some(name) = ancestor.file_name().map(|n| n.to_os_string()) else {
            return path.to_path_buf();
        };
        tail.push(name);
        if !ancestor.pop() {
            return path.to_path_buf();
        }
    }
}

/// The key two names share exactly when they could be one file on some filesystem this ships to.
///
/// The filesystems differ on which names are distinct: ext4 keeps every byte sequence apart,
/// APFS and NTFS fold case, APFS also folds Unicode normalization. So a name comparison that
/// only lowercases ASCII answers for one of them — `Wiki`/`wiki` pairs, `Café`/`café` does not,
/// and the NFC and NFD spellings of a Korean name do not, which is exactly the vocabulary this
/// vault's directories are named in. Folded by Unicode case AND NFC, so the answer is the union
/// of what any of them would collapse rather than one platform's — one answer for a vault that
/// syncs between them, rather than a verdict that changes with the machine it ran on.
///
/// A key rather than only a predicate, so a set of addresses can be looked up in one hash
/// instead of scanned pairwise. `/` is unaffected by either fold, so this applies to a whole
/// path exactly as it does to one segment.
pub fn fold_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.to_lowercase().nfc().collect()
}

/// Whether two directory-entry names could be one file on some filesystem this ships to.
/// The predicate form of [`fold_name`]; both answer from the one rule.
pub fn names_fold_together(a: &str, b: &str) -> bool {
    fold_name(a) == fold_name(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_publication_reuses_exact_bytes_and_refuses_incomplete_or_corrupt_input() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("original");
        let captured = crate::captured_file::CapturedFile::capture(&b"original"[..], 8).unwrap();
        assert_eq!(
            publish_exclusive_from(&path, captured.reader().unwrap(), Some(0o600)).unwrap(),
            PublishOutcome::Created
        );
        assert_eq!(
            publish_exclusive_from(&path, captured.reader().unwrap(), Some(0o600)).unwrap(),
            PublishOutcome::Reused
        );
        assert_eq!(
            publish_exclusive_from(&path, &b"different"[..], Some(0o600)).unwrap(),
            PublishOutcome::Occupied
        );
        std::fs::write(captured.path(), b"tampered").unwrap();
        let absent = root.path().join("absent");
        assert!(publish_exclusive_from(&absent, captured.reader().unwrap(), Some(0o600)).is_err());
        assert!(!absent.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
    use tempfile::TempDir;

    #[test]
    fn exclusive_publication_reuses_equal_bytes_and_preserves_other_entries() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("original.md");
        assert_eq!(
            publish_exclusive(&path, b"original", None).unwrap(),
            PublishOutcome::Created
        );
        assert_eq!(
            publish_exclusive(&path, b"original", None).unwrap(),
            PublishOutcome::Reused
        );
        assert_eq!(
            publish_exclusive(&path, b"different", None).unwrap(),
            PublishOutcome::Occupied
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        let directory = dir.path().join("directory.md");
        std::fs::create_dir(&directory).unwrap();
        assert_eq!(
            publish_exclusive(&directory, b"different", None).unwrap(),
            PublishOutcome::Occupied
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn exclusive_publication_supports_a_maximum_length_destination_name() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(format!("{}.md", "a".repeat(252)));
        assert_eq!(
            publish_exclusive(&path, b"whole", Some(0o600)).unwrap(),
            PublishOutcome::Created
        );
        assert_eq!(
            publish_exclusive(&path, b"whole", None).unwrap(),
            PublishOutcome::Reused
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn exclusive_publication_reports_io_errors_without_treating_them_as_absence() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("not-a-directory");
        std::fs::write(&parent, b"keep").unwrap();
        assert!(publish_exclusive(&parent.join("article.md"), b"new", None).is_err());
        assert_eq!(std::fs::read(&parent).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_exclusive_publishers_never_replace_the_winner() {
        use std::sync::{Arc, Barrier};
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("original.md");
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|number| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let bytes = vec![b'a' + number; 32_768];
                    barrier.wait();
                    (
                        bytes.clone(),
                        publish_exclusive(&path, &bytes, None).unwrap(),
                    )
                })
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<_> = outcomes
            .iter()
            .filter(|(_, o)| *o == PublishOutcome::Created)
            .collect();
        assert_eq!(winners.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), winners[0].0);
        assert!(
            outcomes
                .iter()
                .all(|(_, o)| matches!(o, PublishOutcome::Created | PublishOutcome::Occupied))
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_equal_publications_share_one_original() {
        use std::sync::{Arc, Barrier};
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("same.md");
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    publish_exclusive(&path, b"same bytes", None).unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| **o == PublishOutcome::Created)
                .count(),
            1
        );
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, PublishOutcome::Created | PublishOutcome::Reused))
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_publication_neither_reuses_nor_replaces_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("outside");
        std::fs::write(&target, b"same").unwrap();
        let alias = dir.path().join("alias.md");
        symlink(&target, &alias).unwrap();
        assert_eq!(
            publish_exclusive(&alias, b"same", Some(0o600)).unwrap(),
            PublishOutcome::Occupied
        );
        assert!(alias.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"same");
        let dangling = dir.path().join("dangling.md");
        symlink(dir.path().join("missing"), &dangling).unwrap();
        assert_eq!(
            publish_exclusive(&dangling, b"new", None).unwrap(),
            PublishOutcome::Occupied
        );
        assert!(!dir.path().join("missing").exists());
        let secret = dir.path().join("secret");
        publish_exclusive(&secret, b"secret", Some(0o600)).unwrap();
        assert_eq!(
            secret.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        let inode = std::os::unix::fs::MetadataExt::ino(&secret.metadata().unwrap());
        publish_exclusive(&secret, b"secret", Some(0o644)).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&secret.metadata().unwrap()),
            inode
        );
        assert_eq!(
            secret.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_concurrently_created_symlink_is_not_overwritten() {
        use std::sync::{Arc, Barrier};
        let dir = TempDir::new().unwrap();
        let external = dir.path().join("external");
        std::fs::write(&external, b"external original").unwrap();
        let destination = dir.path().join("article.md");
        let barrier = Arc::new(Barrier::new(2));
        let alias_destination = destination.clone();
        let alias_target = external.clone();
        let alias_barrier = barrier.clone();
        let alias = std::thread::spawn(move || {
            alias_barrier.wait();
            std::os::unix::fs::symlink(alias_target, alias_destination)
        });
        barrier.wait();
        let outcome = publish_exclusive(&destination, b"fetched original", None).unwrap();
        match alias.join().unwrap() {
            Ok(()) => {
                assert_eq!(outcome, PublishOutcome::Occupied);
                assert!(
                    destination
                        .symlink_metadata()
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            }
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                assert_eq!(outcome, PublishOutcome::Created);
                assert_eq!(std::fs::read(destination).unwrap(), b"fetched original");
            }
        }
        assert_eq!(std::fs::read(external).unwrap(), b"external original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn writes_contents_and_leaves_no_temp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("data.json");
        write_atomic(&path, b"hello", None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        // No stranded temp.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "temp must not linger");
    }

    #[test]
    fn temp_keeps_extension_suffix_for_sweeps() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("run.jsonl");
        let (temporary, _file) = create_temporary_file(|| atomic_name(&path), 0o600).unwrap();
        assert!(
            temporary
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".jsonl.tmp")
        );
    }

    #[cfg(unix)]
    #[test]
    fn temporary_creation_preserves_occupied_symlinks_and_restricts_access_before_writes() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = TempDir::new().unwrap();
        let victim = dir.path().join("untouched");
        std::fs::write(&victim, b"original bytes").unwrap();
        let occupied = dir.path().join("occupied.tmp");
        symlink(&victim, &occupied).unwrap();
        let available = dir.path().join("available.tmp");
        let mut names = [occupied.clone(), available.clone()].into_iter();
        let (temporary, file) = create_temporary_file(|| names.next().unwrap(), 0o600).unwrap();
        assert_eq!(temporary.0, available);
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(file.metadata().unwrap().len(), 0);
        assert_eq!(std::fs::read(&victim).unwrap(), b"original bytes");
        drop(file);
        drop(temporary);
        assert!(std::fs::symlink_metadata(occupied).unwrap().is_symlink());
        assert!(!available.exists());
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.json");
        write_atomic(&path, b"v1", None).unwrap();
        write_atomic(&path, b"v2", None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
    }

    #[cfg(unix)]
    #[test]
    fn applies_mode_to_published_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("secret.json");
        write_atomic(&path, b"token", Some(0o600)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be owner-only");
    }

    #[test]
    fn rename_failure_leaves_no_temp() {
        // Force failure by making the final path a directory: rename onto it fails.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("busy.json");
        std::fs::create_dir(&path).unwrap();
        assert!(write_atomic(&path, b"x", None).is_err());
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftover.is_empty(),
            "temp must be cleaned up on rename failure"
        );
    }
    /// The three ways a filesystem this ships to collapses two names, and the one it does not.
    /// Written as a table rather than one case because ASCII case folding alone is what the
    /// caller used to do, and it is the pairs BEYOND ASCII that were being missed.
    #[test]
    fn names_that_some_shipped_filesystem_would_collapse_are_paired() {
        for (a, b) in [
            ("wiki", "Wiki"),
            ("café", "Café"),
            ("caf\u{e9}", "cafe\u{301}"),
            (
                "\u{d55c}\u{ad6d}\u{c5b4}",
                "\u{1112}\u{1161}\u{11ab}\u{1100}\u{116e}\u{11a8}\u{110b}\u{1165}",
            ),
        ] {
            assert!(names_fold_together(a, b), "{a:?} and {b:?} can be one file");
        }
        for (a, b) in [("wiki", "wiki-archive"), ("daily", "weekly"), ("me", "men")] {
            assert!(
                !names_fold_together(a, b),
                "{a:?} and {b:?} are two files everywhere"
            );
        }
    }

    /// `canonicalize` needs the whole path to exist, and the paths this compares are frequently
    /// about to be created. Resolving the longest existing ancestor keeps both sides on one
    /// basis: a symlinked parent with an absent child still resolves through the symlink, which
    /// is what makes an overlap visible BEFORE the first write creates it.
    #[test]
    fn a_missing_child_still_resolves_through_a_symlinked_parent() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        #[cfg(unix)]
        {
            let alias = dir.path().join("alias");
            if std::os::unix::fs::symlink(&real, &alias).is_ok() {
                assert_eq!(
                    canonical_prefix(&alias.join("new")),
                    canonical_prefix(&real).join("new")
                );
            }
        }
        assert_eq!(canonical_prefix(&real), real.canonicalize().unwrap());
    }
}
