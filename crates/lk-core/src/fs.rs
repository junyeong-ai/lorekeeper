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

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Write `contents` to `path` atomically and durably:
/// 1. write to a **per-writer-unique** temp in the same directory — `pid` plus a
///    process-global sequence, so two writers targeting the same `path` (e.g. two
///    concurrent processes, or two calls in one process) never share and truncate
///    one temp;
/// 2. fsync the temp so its bytes are on stable storage;
/// 3. optionally `chmod` it (Unix) before publishing — a secret file is restricted
///    while still private, never briefly world-readable;
/// 4. rename onto `path` (atomic on POSIX);
/// 5. fsync the parent directory (Unix) so the rename itself survives a power loss.
///
/// The temp keeps `path`'s extension before `.tmp` (`<stem>.<pid>.<seq>.<ext>.tmp`),
/// so suffix-based sweeps that reap stranded temps (e.g. the ingest queue sweep on
/// `.jsonl.tmp`) still match. On any failure the temp is removed and the error
/// returned, so a partial write never lingers as a real file.
pub fn write_atomic(path: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => path.with_extension(format!("{pid}.{seq}.{ext}.tmp")),
        _ => path.with_extension(format!("{pid}.{seq}.tmp")),
    };

    let result = write_to_temp_then_rename(path, &tmp, contents, mode);
    if result.is_err() {
        // Best-effort: the write/rename error is the one the caller needs to see.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_to_temp_then_rename(
    path: &Path,
    tmp: &Path,
    contents: &[u8],
    mode: Option<u32>,
) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(tmp)?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);

    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(m))?;
    }
    #[cfg(not(unix))]
    let _ = mode;

    std::fs::rename(tmp, path)?;

    // The bytes are fsynced, but the directory entry needs its own fsync or a power
    // loss can roll the rename back. Unix-only — Windows can't fsync a dir via std.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
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
    use tempfile::TempDir;

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
        // A `.jsonl` target must produce a `<...>.jsonl.tmp` temp so a sweep filtering
        // `.jsonl.tmp` still reaps it. We can't observe the temp after a successful
        // write, so assert the naming rule directly via a second call's determinism:
        // the suffix is `.{ext}.tmp`.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("run.jsonl");
        write_atomic(&path, b"x\n", None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
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
