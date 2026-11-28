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

/// The environment variable naming the file a child command writes its failure to.
pub(crate) const CHILD_FAILURE_ENV: &str = "LORE_CHILD_FAILURE_REPORT";

/// A failed child command's classified failure, written where `CHILD_FAILURE_ENV` names.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChildFailure {
    pub kind: FailureKind,
    pub detail: String,
}

/// Writes `error`'s classification for a parent that named `CHILD_FAILURE_ENV`. The parent
/// reads a missing or unreadable report as a fault, so a write that fails can only make it
/// stricter.
pub(crate) fn report_child_failure(error: &miette::Report) {
    if let Some(path) = std::env::var_os(CHILD_FAILURE_ENV) {
        write_child_failure(std::path::Path::new(&path), error);
    }
}

fn write_child_failure(path: &std::path::Path, error: &miette::Report) {
    let failure = ChildFailure {
        kind: FailureKind::of(error),
        detail: error.to_string(),
    };
    if let Ok(bytes) = serde_json::to_vec(&failure) {
        let _ = std::fs::write(path, bytes);
    }
}

/// The failure a child wrote to `path`: none when it wrote nothing or something else.
#[cfg(unix)]
pub(crate) fn load_child_failure(path: &std::path::Path) -> Option<ChildFailure> {
    let bytes = std::fs::read(path).ok()?;
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
    fn a_child_reports_its_failure_kind_and_a_missing_report_reads_as_none() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("failure.json");
        assert!(load_child_failure(&path).is_none());
        std::fs::write(&path, "").unwrap();
        assert!(
            load_child_failure(&path).is_none(),
            "an empty report is no report"
        );
        let drift = crate::commands::typed_error(lk_graph::GraphError::AuditSubjectRemoved(
            "wiki/concepts/a.md".into(),
        ));
        write_child_failure(&path, &drift);
        let reported = load_child_failure(&path).unwrap();
        assert_eq!(reported.kind, FailureKind::InputsChanged);
        assert_eq!(reported.detail, drift.to_string());
        write_child_failure(&path, &miette::miette!("unreadable page"));
        assert_eq!(load_child_failure(&path).unwrap().kind, FailureKind::Fault);
    }
}
