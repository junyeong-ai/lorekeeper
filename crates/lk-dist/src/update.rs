use std::cmp::Ordering;
use std::path::Path;

use semver::Version;

use crate::DistError;

/// What replacing the binary would amount to.
///
/// Decided from versions alone — no I/O, no network — so the rule that governs whether an
/// update happens is the part that can be exhausted by tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    AlreadyCurrent(Version),
    Replace {
        from: Version,
        to: Version,
    },
    /// The release that answered is older than the running binary.
    ///
    /// Refused rather than performed, because this is what a yanked or rolled-back release
    /// looks like — and what a stale answer from the trailing web view looks like, which is
    /// the same shape and arrives without warning. Going back is a deliberate act, so it takes
    /// a version named on the command line.
    RefusedDowngrade {
        running: Version,
        offered: Version,
    },
}

/// `requested` is a version named on the command line, which is deliberate in either
/// direction; `latest` is what the release channel answered, which is only ever followed
/// forward.
pub fn decide(
    running: &Version,
    requested: Option<&Version>,
    latest: Option<&Version>,
    force: bool,
) -> Option<Decision> {
    let target = requested.or(latest)?;

    Some(match precedence(target, running) {
        Ordering::Less if requested.is_none() => Decision::RefusedDowngrade {
            running: running.clone(),
            offered: target.clone(),
        },
        Ordering::Equal if !force => Decision::AlreadyCurrent(running.clone()),
        _ => Decision::Replace {
            from: running.clone(),
            to: target.clone(),
        },
    })
}

/// Semver precedence, which is not `Version`'s own ordering.
///
/// `Ord` has to agree with `Eq`, and `Eq` separates two builds of one version — so `Version`
/// orders by build metadata, which the specification says carries no precedence. Comparing the
/// pair with that field cleared is the crate's ordering for everything that does count, minus
/// the one field that must not: without it `1.2.3+build.5` reads as neither older than nor the
/// same as `1.2.3`, which resolves to an update onto the version already installed.
fn precedence(a: &Version, b: &Version) -> Ordering {
    fn bare(version: &Version) -> Version {
        Version {
            build: semver::BuildMetadata::EMPTY,
            ..version.clone()
        }
    }
    bare(a).cmp(&bare(b))
}

/// The version a binary reports when asked.
///
/// Executing it is the only check that answers the question an update actually has — does the
/// file that just landed run, on this kernel, with this architecture, past this platform's code
/// signing — and it answers all of them at once. Comparing bytes or reading a header would
/// each answer one.
pub fn running_version(binary: &Path) -> Result<Version, DistError> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| DistError::io(format!("running {}", binary.display()), e))?;
    if !output.status.success() {
        return Err(DistError::Integrity(format!(
            "{} exited {} when asked for its version",
            binary.display(),
            output.status
        )));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    let token = printed
        .split_whitespace()
        .next_back()
        .ok_or_else(|| DistError::Integrity(format!("{} printed no version", binary.display())))?;
    Version::parse(token).map_err(|e| {
        DistError::Integrity(format!(
            "{} printed `{token}`, which is not a version: {e}",
            binary.display()
        ))
    })
}

/// Put `bytes` at `dest`, prove the result runs and names `expected`, and restore what was
/// there if it does not.
///
/// The publish is `lk_core::fs::write_atomic` — the workspace's one temp-fsync-rename writer —
/// so the destination is never a partial file, and on Unix the kernel keeps the running
/// binary's inode alive until this process exits. What follows the rename is the part that
/// matters: an installation whose binary does not run has no way left to repair itself, so the
/// previous bytes are held until the new ones have proven they do.
pub fn install_binary(bytes: &[u8], dest: &Path, expected: &Version) -> Result<(), DistError> {
    let previous =
        std::fs::read(dest).map_err(|e| DistError::io(format!("reading {}", dest.display()), e))?;
    let mode = executable_mode(dest);

    publish(bytes, dest, mode)?;

    match running_version(dest) {
        Ok(found) if &found == expected => Ok(()),
        outcome => {
            // A restore that fails leaves a downloaded binary in place that does not run, and
            // the operator needs to be told that BEFORE the write error — the write error alone
            // reads as "nothing happened", which is the one thing that is not true.
            if let Err(restore) = publish(&previous, dest, mode) {
                return Err(DistError::Integrity(format!(
                    "the downloaded binary at {} does not run as {expected}, and putting the \
                     previous one back also failed ({restore}) — that path now holds a binary \
                     that does not work",
                    dest.display()
                )));
            }
            Err(match outcome {
                Ok(found) => DistError::Integrity(format!(
                    "the downloaded binary reports {found}, not {expected}; \
                     the previous binary was put back"
                )),
                Err(e) => DistError::Integrity(format!(
                    "the downloaded binary did not run ({e}); the previous binary was put back"
                )),
            })
        }
    }
}

fn publish(bytes: &[u8], dest: &Path, mode: Option<u32>) -> Result<(), DistError> {
    lk_core::fs::write_atomic(dest, bytes, mode).map_err(|e| {
        let context = match e.kind() {
            std::io::ErrorKind::PermissionDenied => format!(
                "cannot write {} — the directory belongs to another user; \
                 re-run with the privileges that installed it",
                dest.display()
            ),
            _ => format!("writing {}", dest.display()),
        };
        DistError::io(context, e)
    })
}

/// The permissions the binary already carries, so a replacement keeps them rather than
/// acquiring whatever the umask grants a new file. A destination with none — which cannot be
/// the running binary — takes the mode an installer would give it.
#[cfg(unix)]
fn executable_mode(dest: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(
        std::fs::metadata(dest)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o755),
    )
}

#[cfg(not(unix))]
fn executable_mode(_dest: &Path) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn the_same_version_is_already_current() {
        assert_eq!(
            decide(&v("0.20.0"), None, Some(&v("0.20.0")), false),
            Some(Decision::AlreadyCurrent(v("0.20.0")))
        );
    }

    #[test]
    fn force_replaces_the_same_version() {
        assert_eq!(
            decide(&v("0.20.0"), None, Some(&v("0.20.0")), true),
            Some(Decision::Replace {
                from: v("0.20.0"),
                to: v("0.20.0"),
            })
        );
    }

    #[test]
    fn a_newer_release_is_replaced_into() {
        assert_eq!(
            decide(&v("0.20.0"), None, Some(&v("0.21.0")), false),
            Some(Decision::Replace {
                from: v("0.20.0"),
                to: v("0.21.0"),
            })
        );
    }

    /// A rolled-back release and a stale answer from the trailing web view are the same shape,
    /// and neither is a reason to install older code without being asked.
    #[test]
    fn a_channel_that_answers_older_is_refused() {
        assert_eq!(
            decide(&v("0.21.0"), None, Some(&v("0.20.0")), false),
            Some(Decision::RefusedDowngrade {
                running: v("0.21.0"),
                offered: v("0.20.0"),
            })
        );
    }

    #[test]
    fn a_named_version_goes_back_when_that_is_what_was_named() {
        assert_eq!(
            decide(&v("0.21.0"), Some(&v("0.20.0")), Some(&v("0.21.0")), false),
            Some(Decision::Replace {
                from: v("0.21.0"),
                to: v("0.20.0"),
            })
        );
    }

    /// Precedence is semver's, not the string's: `0.3.10` is above `0.3.9`, and a final
    /// release is above its own pre-release — so upgrading off `1.2.3-rc.1` onto `1.2.3` is the
    /// upgrade it is, never a refused downgrade.
    #[test]
    fn precedence_is_semver_precedence() {
        assert!(matches!(
            decide(&v("0.3.9"), None, Some(&v("0.3.10")), false),
            Some(Decision::Replace { .. })
        ));
        assert!(matches!(
            decide(&v("1.2.3-rc.1"), None, Some(&v("1.2.3")), false),
            Some(Decision::Replace { .. })
        ));
        assert!(matches!(
            decide(&v("1.2.3"), None, Some(&v("1.2.3-rc.2")), false),
            Some(Decision::RefusedDowngrade { .. })
        ));
        // Build metadata carries no precedence, so it is the same version.
        assert_eq!(
            decide(&v("1.2.3"), None, Some(&v("1.2.3+build.5")), false),
            Some(Decision::AlreadyCurrent(v("1.2.3")))
        );
    }

    #[test]
    fn nothing_to_decide_without_a_version_from_either_side() {
        assert_eq!(decide(&v("0.20.0"), None, None, false), None);
    }

    #[test]
    fn a_binary_that_does_not_run_leaves_the_previous_one_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("lore");
        std::fs::write(&dest, b"#!/bin/sh\necho 'lore 0.20.0'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let err = install_binary(b"not an executable at all", &dest, &v("0.21.0"))
            .expect_err("a binary that cannot report its version must not be installed");
        assert!(matches!(err, DistError::Integrity(_)), "{err}");
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"#!/bin/sh\necho 'lore 0.20.0'\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_reporting_another_version_is_rolled_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("lore");
        let previous = b"#!/bin/sh\necho 'lore 0.20.0'\n";
        std::fs::write(&dest, previous).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = install_binary(b"#!/bin/sh\necho 'lore 0.19.0'\n", &dest, &v("0.21.0"))
            .expect_err("a binary that is not the requested version must not be installed");
        assert!(matches!(err, DistError::Integrity(_)), "{err}");
        assert_eq!(std::fs::read(&dest).unwrap(), previous);
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_that_reports_the_requested_version_is_installed_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("lore");
        std::fs::write(&dest, b"#!/bin/sh\necho 'lore 0.20.0'\n").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();

        install_binary(b"#!/bin/sh\necho 'lore 0.21.0'\n", &dest, &v("0.21.0")).unwrap();
        assert_eq!(running_version(&dest).unwrap(), v("0.21.0"));
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "the replacement keeps its own bits");
    }
}
