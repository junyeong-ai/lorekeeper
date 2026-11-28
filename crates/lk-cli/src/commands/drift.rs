//! The one classification of a failure that stops work: input drift, which a later run plans
//! around without an operator, or a fault. A scheduled run's halt decision reads it in process
//! (`run::Runner::failure`), and a child command the run starts reports it on the child wire
//! (`ChildFailure`), so both sides classify the same typed errors with the same function.

use serde::{Deserialize, Serialize};

/// How a failure that stopped work reads for the operation that scheduled it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FailureKind {
    /// State the operator repairs (accounting, integrity, configuration, an unknown host
    /// failure): a scheduled operation pauses until `--resume`.
    Fault,
    /// A writer changed or removed inputs while the work read them (`is_input_drift`): the
    /// batch ends with its unfinished work retained, and the next run plans again.
    InputsChanged,
}

impl FailureKind {
    pub(crate) fn of(error: &miette::Report) -> Self {
        if is_input_drift(error) {
            Self::InputsChanged
        } else {
            Self::Fault
        }
    }
}

/// Whether `error` is input drift: a page, citation, task or capture a writer edited or
/// removed after the job read it, or a publication guard that found guarded bytes changed.
/// The halt decision reads only these typed errors. Integrity faults (an immutable capture
/// missing or changed, corrupt state, an unreadable present page), host failures and
/// configuration or root drift are not drift.
pub(crate) fn is_input_drift(error: &miette::Report) -> bool {
    use lk_graph::GraphError;
    let vault_drift = |error: &lk_vault::VaultError| {
        matches!(
            error,
            lk_vault::VaultError::MutationConflict(_) | lk_vault::VaultError::RssOriginChanged(_)
        )
    };
    if let Some(rejection) =
        error.downcast_ref::<crate::commands::queue::application::PublicationRejection>()
    {
        return rejection.kind.is_input_change();
    }
    error
        .downcast_ref::<crate::commands::work::SourceChanged>()
        .is_some()
        || error
            .downcast_ref::<crate::commands::queue::TaskNotCurrent>()
            .is_some_and(crate::commands::queue::TaskNotCurrent::is_input_change)
        || error
            .downcast_ref::<lk_vault::VaultError>()
            .is_some_and(vault_drift)
        || error
            .downcast_ref::<GraphError>()
            .is_some_and(|error| match error {
                GraphError::AuditInputChanged(_)
                | GraphError::AuditScreeningChanged
                | GraphError::AuditSubjectRemoved(_)
                | GraphError::AuditSubjectInputChanged { .. } => true,
                GraphError::Vault(error) => vault_drift(error),
                _ => false,
            })
}

/// The environment variable naming the report a native run created for one child command.
/// Every process Lore starts goes without it (`lk_core::process::command`); the run sets it
/// on the one child it names.
pub(crate) const CHILD_FAILURE_ENV: &str = lk_core::process::CHILD_FAILURE_REPORT_ENV;

/// The most report bytes a run reads back; a larger report reads as none, a fault.
#[cfg(unix)]
const MAX_CHILD_FAILURE_BYTES: u64 = 1024 * 1024;

/// A failed child command's classified failure, written to the report its run created.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChildFailure {
    pub kind: FailureKind,
    pub detail: String,
}

/// Writes `error`'s classification into the report a parent named in `CHILD_FAILURE_ENV`. The
/// parent created that file and reads it back through its own descriptor, so a missing,
/// replaced or unwritable report reads as a fault there and can only make it stricter.
pub(crate) fn report_child_failure(error: &miette::Report) {
    if let Some(path) = std::env::var_os(CHILD_FAILURE_ENV) {
        write_child_failure(std::path::Path::new(&path), error);
    }
}

fn write_child_failure(path: &std::path::Path, error: &miette::Report) {
    use std::io::Write as _;
    let failure = ChildFailure {
        kind: FailureKind::of(error),
        detail: error.to_string(),
    };
    if let (Ok(bytes), Ok(mut report)) = (serde_json::to_vec(&failure), open_report(path)) {
        let _ = report.set_len(0).and_then(|()| report.write_all(&bytes));
    }
}

/// Opens the report the parent created, never creating one or following a link to another
/// file; a FIFO without a reader fails at once instead of blocking the exit.
#[cfg(unix)]
fn open_report(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let report = std::fs::File::from(rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if !report.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "child failure report is not a regular file",
        ));
    }
    Ok(report)
}

#[cfg(not(unix))]
fn open_report(_: &std::path::Path) -> std::io::Result<std::fs::File> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// The failure a child wrote into `report`, the file the run created and kept open: none when
/// it wrote nothing or something else.
#[cfg(unix)]
pub(crate) fn load_child_failure(report: &mut std::fs::File) -> Option<ChildFailure> {
    use std::io::{Read as _, Seek as _};
    report.rewind().ok()?;
    let mut bytes = Vec::new();
    report
        .take(MAX_CHILD_FAILURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CHILD_FAILURE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_typed_input_drift_is_drift() {
        use crate::commands::queue::application::{RejectionKind, rejection};
        use lk_graph::GraphError;
        let path = || "wiki/concepts/a.md".to_owned();
        for drift in [
            miette::Report::new(crate::commands::work::SourceChanged::new("edited")),
            rejection(RejectionKind::SemanticContextChanged, "changed"),
            miette::Report::new(crate::commands::queue::TaskNotCurrent {
                status: Some(crate::commands::queue::TaskStatus::Stale),
                detail: path(),
            }),
            crate::commands::typed_error(GraphError::AuditInputChanged(path())),
            crate::commands::typed_error(GraphError::AuditScreeningChanged),
            crate::commands::typed_error(GraphError::AuditSubjectRemoved(path())),
            crate::commands::typed_error(GraphError::AuditSubjectInputChanged {
                subject: path(),
                input: path(),
            }),
            crate::commands::typed_error(lk_vault::VaultError::RssOriginChanged(path())),
            crate::commands::typed_error(lk_vault::VaultError::MutationConflict(path().into())),
            crate::commands::typed_error(GraphError::from(lk_vault::VaultError::MutationConflict(
                path().into(),
            ))),
        ] {
            assert!(is_input_drift(&drift), "{drift}");
        }
        for fault in [
            rejection(RejectionKind::InvalidSection, "invalid"),
            rejection(RejectionKind::RetainedWorkInvalid, "unsealed"),
            miette::Report::new(crate::commands::queue::TaskNotCurrent {
                status: Some(crate::commands::queue::TaskStatus::Unreadable),
                detail: path(),
            }),
            miette::Report::new(crate::commands::queue::TaskNotCurrent {
                status: Some(crate::commands::queue::TaskStatus::InvalidTarget),
                detail: path(),
            }),
            crate::commands::typed_error(GraphError::Io("unreadable".into())),
            crate::commands::typed_error(lk_vault::VaultError::Original("missing payload".into())),
            crate::commands::typed_error(lk_vault::VaultError::RetainedCaptureChanged(
                path().into(),
            )),
            miette::miette!("{}", GraphError::AuditSubjectRemoved(path())),
        ] {
            assert!(!is_input_drift(&fault), "{fault}");
        }
    }

    #[test]
    fn a_child_reports_its_failure_into_the_file_its_run_created_and_reads_back() {
        let mut report = tempfile::NamedTempFile::new().unwrap();
        assert!(
            load_child_failure(report.as_file_mut()).is_none(),
            "an empty report is no report"
        );
        let drift = crate::commands::typed_error(lk_graph::GraphError::AuditSubjectRemoved(
            "wiki/concepts/a.md".into(),
        ));
        write_child_failure(report.path(), &drift);
        let reported = load_child_failure(report.as_file_mut()).unwrap();
        assert_eq!(reported.kind, FailureKind::InputsChanged);
        assert_eq!(reported.detail, drift.to_string());
        write_child_failure(report.path(), &miette::miette!("unreadable page"));
        assert_eq!(
            load_child_failure(report.as_file_mut()).unwrap().kind,
            FailureKind::Fault,
            "a shorter report replaces a longer one"
        );

        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.json");
        write_child_failure(&missing, &drift);
        assert!(!missing.exists(), "a child never creates a report");
    }

    #[test]
    fn a_report_path_another_process_replaced_is_never_read_or_followed() {
        let drift = crate::commands::typed_error(lk_graph::GraphError::AuditSubjectRemoved(
            "wiki/concepts/a.md".into(),
        ));
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, "kept").unwrap();
        let mut report = tempfile::NamedTempFile::new().unwrap();
        std::fs::remove_file(report.path()).unwrap();
        std::os::unix::fs::symlink(&target, report.path()).unwrap();
        write_child_failure(report.path(), &drift);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
        assert!(load_child_failure(report.as_file_mut()).is_none());

        std::fs::remove_file(report.path()).unwrap();
        std::fs::write(report.path(), "").unwrap();
        write_child_failure(report.path(), &drift);
        assert!(
            load_child_failure(report.as_file_mut()).is_none(),
            "a report written to a replacement file never reaches the run's own file"
        );
    }
}
