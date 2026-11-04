use std::fs::File;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub enum LockFileAccess {
    Read,
    Write,
    ReadWrite,
}

/// Open a persistent regular lock file without truncation or following a leaf
/// symlink. `create_mode` permits exclusive creation of an absent file; its parent
/// must already exist. This does not acquire a lock or recover protected state.
/// Unix opens are nonblocking so an occupied FIFO cannot strand the caller.
pub fn open_lock_file(
    path: &Path,
    access: LockFileAccess,
    create_mode: Option<u32>,
) -> io::Result<File> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open};
        let access = match access {
            LockFileAccess::Read => OFlags::RDONLY,
            LockFileAccess::Write => OFlags::WRONLY,
            LockFileAccess::ReadWrite => OFlags::RDWR,
        };
        if let Some(mode) = create_mode {
            match open(
                path,
                OFlags::CREATE
                    | OFlags::EXCL
                    | access
                    | OFlags::CLOEXEC
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK,
                Mode::from_raw_mode((mode & 0o7777) as _),
            ) {
                Ok(file) => return Ok(File::from(file)),
                Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        File::from(open(
            path,
            access | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?)
    };
    #[cfg(not(unix))]
    let file = {
        let mut options = File::options();
        match access {
            LockFileAccess::Read => {
                options.read(true);
            }
            LockFileAccess::Write => {
                options.write(true);
            }
            LockFileAccess::ReadWrite => {
                options.read(true).write(true);
            }
        }
        if create_mode.is_some() {
            match File::options().write(true).create_new(true).open(path) {
                Ok(file) => drop(file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        if !std::fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "lock is not a regular file",
            ));
        }
        options.open(path)?
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "lock is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_preserves_bytes_and_requires_explicit_creation_and_existing_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("lock");
        assert!(open_lock_file(&path, LockFileAccess::Read, None).is_err());
        assert!(
            open_lock_file(
                &temporary.path().join("absent/lock"),
                LockFileAccess::Write,
                Some(0o666)
            )
            .is_err()
        );
        assert!(!temporary.path().join("absent").exists());
        open_lock_file(&path, LockFileAccess::Write, Some(0o666)).unwrap();
        std::fs::write(&path, b"retained").unwrap();
        for access in [
            LockFileAccess::Read,
            LockFileAccess::Write,
            LockFileAccess::ReadWrite,
        ] {
            open_lock_file(&path, access, Some(0o600)).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"retained");
        }
    }

    #[cfg(unix)]
    #[test]
    fn access_and_existing_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("lock");
        let restricted = open_lock_file(
            &temporary.path().join("restricted"),
            LockFileAccess::ReadWrite,
            Some(0),
        )
        .unwrap();
        assert_eq!(
            restricted.metadata().unwrap().permissions().mode() & 0o777,
            0
        );
        restricted.try_lock().unwrap();
        std::fs::write(&path, b"retained").unwrap();
        for (mode, access) in [
            (0o200, LockFileAccess::Write),
            (0o400, LockFileAccess::Read),
            (0o600, LockFileAccess::ReadWrite),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let file = open_lock_file(&path, access, Some(0o644)).unwrap();
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, mode);
        }
    }

    #[cfg(unix)]
    #[test]
    fn occupied_special_entries_refuse_without_following_or_waiting() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let regular = temporary.path().join("regular");
        std::fs::write(&regular, b"retained").unwrap();
        let alias = temporary.path().join("alias");
        symlink(&regular, &alias).unwrap();
        let fifo = temporary.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        for path in [&alias, &fifo, temporary.path()] {
            for access in [
                LockFileAccess::Read,
                LockFileAccess::Write,
                LockFileAccess::ReadWrite,
            ] {
                assert!(open_lock_file(path, access, Some(0o666)).is_err());
            }
        }
        assert_eq!(std::fs::read(&regular).unwrap(), b"retained");
    }
}
