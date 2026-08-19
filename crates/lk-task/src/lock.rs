use std::path::Path;

use crate::TaskError;

/// Exclusive hold on the intent plane's stores.
///
/// It guards the PLANE — the board, the transition log, the proposal snapshots, the judged
/// candidates, the day's schedule and the standing reminders — because every one of them is a
/// read-modify-write and any two writers of any of them can drop one of the two. It lives here
/// rather than beside the first command that happened to need it: named for one writer, it
/// guarded `lore task` while `lore ingest` and `lore maintenance` wrote the same files beside
/// it, which is the same defect it already had once when it was named for the board alone.
///
/// Never probed by a caller that already HOLDS it: on the platforms where `File::lock` is an
/// `fcntl` lock rather than `flock`, closing any descriptor for a file drops every lock the
/// process has on it, so the probe's own close would release the hold.
///
/// The kernel's, so a crashed process releases it with no staleness rule to get wrong. It cannot
/// reach across machines — a vault synced by Dropbox or iCloud has two kernels — which is the
/// same limitation an editor on either machine already has.
#[derive(Debug)]
pub struct PlaneLock {
    _file: std::fs::File,
}

impl PlaneLock {
    /// Take the plane, WAITING for whoever holds it.
    ///
    /// A failure is never contention — the wait is what answers that — so it is a plane that can
    /// never be held: a lock path that is not a file this may open, or a filesystem with no
    /// locking at all. Writing anyway loses board lines and transition records while reporting
    /// success, so a caller refuses instead.
    pub fn hold(vault_root: &Path) -> Result<Self, Unholdable> {
        let file = open(vault_root, true)?;
        file.lock()
            .map_err(|e| Unholdable::Filesystem(locking(vault_root, e)))?;
        Ok(Self { _file: file })
    }

    /// Whether the plane could be held at all, without waiting to find out.
    ///
    /// `Ok` however busy it is: a plane another command holds this instant is one this command
    /// would get by waiting, and a reader reporting that as unwritable would call every busy
    /// moment a broken vault. `Err` is the answer that lasts — the same failure [`Self::hold`]
    /// refuses on, which is what lets a view say a write will be refused before one is tried.
    pub fn is_holdable(vault_root: &Path) -> Result<(), Unholdable> {
        // A lock file that is not THERE is the ordinary state of a vault nothing has written
        // yet, not a lasting reason a write would be refused — so the probe does not create it.
        // Sharing the write's own `open` made every read of a fresh vault mint
        // `.lorekeeper/tasks.lock`, and a view run against a mistyped `vault.root` mint the
        // directory: a mutation wearing a reader's name, which is the one thing `survey` and
        // `agenda` are built not to be.
        //
        // Asked of the syscall's own answer rather than of a second `exists()`, which cannot
        // traverse a stray FILE where the directory goes: a regular file at `.lorekeeper` is a
        // lasting refusal, and `exists()` reported it as an empty vault.
        let file = match open(vault_root, false) {
            Ok(file) => file,
            Err(why) if is_absent(&why) => return Ok(()),
            Err(why) => return Err(why),
        };
        match file.try_lock() {
            Ok(_) | Err(std::fs::TryLockError::WouldBlock) => Ok(()),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(Unholdable::Filesystem(locking(vault_root, e)))
            }
        }
    }
}

/// Why the plane cannot be held, as the two things that have different repairs.
///
/// Structural rather than read back out of a message: the caller names a repair from it, and one
/// keyed on matching text in an error string is one that stops matching the day the string is
/// reworded. The same discipline `AtlassianAuth::explain_failure` follows.
#[derive(Debug, thiserror::Error)]
pub enum Unholdable {
    /// The lock FILE could not be opened. Something else sits at that path, or its permissions
    /// are wrong, and the repair is there.
    #[error(
        "{0} — nothing in the intent plane can be changed while it cannot be held, because two \
         commands overlapping would lose board lines and history with nothing to say so. Reading \
         is unaffected. Clear whatever sits at that path, or fix its permissions."
    )]
    Path(TaskError),
    /// `lock` itself failed, which is a filesystem with no locking — it will not start having
    /// any on the next run.
    #[error(
        "{0} — nothing in the intent plane can be changed while it cannot be held, because two \
         commands overlapping would lose board lines and history with nothing to say so. Reading \
         is unaffected. This filesystem cannot lock — move the vault to one that can."
    )]
    Filesystem(TaskError),
}

fn path(vault_root: &Path) -> std::path::PathBuf {
    vault_root.join(".lorekeeper").join("tasks.lock")
}

/// Whether the lock file is simply not there yet — the one open failure a write repairs by
/// creating it, and so the only one that is not a lasting refusal.
fn is_absent(why: &Unholdable) -> bool {
    matches!(
        why,
        Unholdable::Path(TaskError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound
    )
}

fn locking(vault_root: &Path, source: std::io::Error) -> TaskError {
    TaskError::io(format!("lock {}", path(vault_root).display()), source)
}

fn open(vault_root: &Path, create: bool) -> Result<std::fs::File, Unholdable> {
    let path = path(vault_root);
    let dir = path.parent().expect("the lock path has a parent");
    if create {
        std::fs::create_dir_all(dir)
    } else {
        Ok(())
    }
    .and_then(|()| {
        std::fs::OpenOptions::new()
            .create(create)
            .truncate(false)
            .write(true)
            .open(&path)
    })
    .map_err(|e| Unholdable::Path(TaskError::io(format!("open {}", path.display()), e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock path that is not a file this may open can never be held, and saying so is what
    /// stops a write that would lose records from reporting success.
    #[test]
    fn a_plane_that_can_never_be_held_says_so_both_ways() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(PlaneLock::hold(tmp.path()).is_ok());
        assert!(PlaneLock::is_holdable(tmp.path()).is_ok());

        let lock = tmp.path().join(".lorekeeper").join("tasks.lock");
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();

        // And says WHICH failure it is, because the two have different repairs.
        assert!(matches!(
            PlaneLock::hold(tmp.path()),
            Err(Unholdable::Path(_))
        ));
        assert!(matches!(
            PlaneLock::is_holdable(tmp.path()),
            Err(Unholdable::Path(_))
        ));
    }

    /// A stray FILE where the lock's directory goes is a lasting refusal, and the probe read it
    /// as a vault nothing had written yet — `exists()` cannot traverse through it, so it
    /// answered the same "not there" a fresh vault does.
    #[test]
    fn something_sitting_where_the_lock_goes_is_not_an_empty_vault() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            PlaneLock::is_holdable(tmp.path()).is_ok(),
            "nothing written yet"
        );

        std::fs::write(tmp.path().join(".lorekeeper"), b"").unwrap();
        assert!(matches!(
            PlaneLock::is_holdable(tmp.path()),
            Err(Unholdable::Path(_))
        ));
        assert!(matches!(
            PlaneLock::hold(tmp.path()),
            Err(Unholdable::Path(_))
        ));
    }

    /// A plane another command holds this instant is one a caller would get by waiting, so a
    /// view must not report it as a vault nothing can write.
    #[test]
    fn a_plane_someone_else_holds_is_still_holdable() {
        let tmp = tempfile::tempdir().unwrap();
        let held = PlaneLock::hold(tmp.path()).unwrap();
        assert!(PlaneLock::is_holdable(tmp.path()).is_ok());
        drop(held);
    }
}
