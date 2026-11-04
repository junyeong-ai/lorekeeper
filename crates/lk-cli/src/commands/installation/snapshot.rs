use std::ffi::OsString;
use std::path::PathBuf;

use crate::commands::GlobalOptions;

pub(super) fn run(
    opts: &GlobalOptions,
    data_dir: Option<PathBuf>,
    command: Vec<OsString>,
) -> miette::Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (opts, data_dir, command);
        Err(miette::miette!(
            "snapshot exclusion requires Unix; Windows is unsupported"
        ))
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        use std::os::unix::process::CommandExt;

        use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};

        let (program, arguments) = command
            .split_first()
            .ok_or_else(|| miette::miette!("snapshot requires a command after --"))?;
        let protection =
            super::Protection::snapshot(opts, super::installation(data_dir.clone())?, data_dir)?;
        let root = protection
            .schema_root()?
            .ok_or_else(|| miette::miette!("snapshot has no protected vault"))?;
        let (mutation, queue) = protection
            .claim_publication()?
            .ok_or_else(|| miette::miette!("snapshot has no publication exclusion"))?;
        match std::fs::symlink_metadata(root.join(".lorekeeper/mutation/pending.json")) {
            Ok(_) => {
                return Err(miette::miette!(
                    "pending mutation journal; snapshot does not recover unfinished writes"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(miette::miette!(
                    "inspect snapshot mutation journal: {error}"
                ));
            }
        }
        let mut child = std::process::Command::new(program);
        child.args(arguments);
        protection.configure_snapshot(&mut child)?;
        for descriptor in std::iter::once(mutation.as_fd()).chain(queue.as_ref().map(AsFd::as_fd)) {
            let flags = fcntl_getfd(descriptor)
                .map_err(|error| miette::miette!("inspect snapshot descriptor: {error}"))?;
            fcntl_setfd(descriptor, flags & !FdFlags::CLOEXEC)
                .map_err(|error| miette::miette!("inherit snapshot descriptor: {error}"))?;
        }
        let error = child.exec();
        Err(miette::miette!(
            "exec snapshot command {program:?}: {error}"
        ))
    }
}
