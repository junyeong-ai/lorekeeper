//! Private creation with owner access preserved across caller umasks.

use std::path::Path;

pub(super) fn create(path: &Path) -> std::io::Result<()> {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn temporary(prefix: &str) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn created_directories_are_private_and_exclusive() {
        if std::env::var_os("LORE_PRIVATE_DIRECTORY_TEST_CHILD").is_none() {
            for mask in ["000", "777"] {
                let output = std::process::Command::new("/bin/sh")
                .args(["-c", "umask \"$1\"; shift; exec \"$@\"", "private-directory-test", mask])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", "commands::private_directory::tests::created_directories_are_private_and_exclusive"])
                .env("LORE_PRIVATE_DIRECTORY_TEST_CHILD", "1")
                .output()
                .unwrap();
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let scratch = temporary("lore-private-directory-test-").unwrap();
        let directory = scratch.path().join("persistent");
        create(&directory).unwrap();
        for path in [scratch.path(), directory.as_path()] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let retained = directory.join("retained");
        std::fs::write(&retained, "existing runtime").unwrap();
        std::fs::set_permissions(&retained, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            create(&directory).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read_to_string(retained).unwrap(),
            "existing runtime"
        );
    }
}
