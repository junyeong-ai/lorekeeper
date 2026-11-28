//! Starting child processes. `command` is the one process constructor (the workspace's
//! `clippy.toml` disallows the others), so a variable addressed to one process never reaches a
//! process it starts.

use std::ffi::OsStr;

/// The file a native run names for one Lore child's classified failure (lk-cli
/// `drift::ChildFailure`). It addresses that child alone: no process Lore starts inherits it,
/// and the run sets it again on the one child it names.
pub const CHILD_FAILURE_REPORT_ENV: &str = "LORE_CHILD_FAILURE_REPORT";

/// A process to start, without the variables that address only the process Lore was given.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[expect(clippy::disallowed_methods, reason = "the one process constructor")]
    let mut command = std::process::Command::new(program);
    command.env_remove(CHILD_FAILURE_REPORT_ENV);
    command
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn a_started_process_never_inherits_a_process_addressed_variable() {
        let mut command = super::command("/usr/bin/env");
        command.env(super::CHILD_FAILURE_REPORT_ENV, "set by the parent");
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == super::CHILD_FAILURE_REPORT_ENV && value.is_some()),
            "a caller can still name the one child it addresses"
        );
        let scrubbed = super::command("/usr/bin/env");
        assert!(
            scrubbed
                .get_envs()
                .any(|(name, value)| name == super::CHILD_FAILURE_REPORT_ENV && value.is_none()),
            "every other process starts without it"
        );
    }
}
