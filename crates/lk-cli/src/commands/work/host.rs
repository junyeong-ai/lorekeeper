//! Whether a request whose invocation ended without an answer is still held.
//!
//! An unanswered failure (an exit, a timeout or a missing result) cannot tell a host fault
//! from a request the host cannot serve. Its hold therefore lasts only until the same engine
//! answers a later request: that answer shows the host serves requests, so the held request is
//! retried once. Failing again after that answer exhausts it until an explicit retry. While no
//! engine answers, one run with no admissible work releases the oldest pending hold as the
//! probe an operator would send; a probe that fails again only renews its hold.

use std::collections::BTreeMap;
use std::path::Path;

use jiff::Timestamp;
use lk_core::execution::AgentEngine;
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

use super::state::InvocationFailure;

const PATH: &str = ".lorekeeper/queue/host-answers.json";
const SCHEMA: &str = "host-answers-v1";
const MAX_BYTES: usize = 4096;

/// The retained hold an unanswered failure created, named by the store that keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "store", rename_all = "kebab-case", deny_unknown_fields)]
pub(in crate::commands) enum HoldIdentity {
    Inspection { work_id: String, node_id: String },
    Generation { task_id: String, cache_hash: String },
    Screening { task_id: String, cache_hash: String },
    Preparation { key: super::preparation::Key },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    identity: HoldIdentity,
    failed_at: Option<Timestamp>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: String,
    answers: BTreeMap<AgentEngine, Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probe: Option<Probe>,
}

/// The latest answer each engine gave, and the one pending hold released as a probe.
#[derive(Debug, Default)]
pub(in crate::commands) struct HostAnswers {
    answers: BTreeMap<AgentEngine, Timestamp>,
    probe: Option<Probe>,
}

/// What a retained invocation failure means for its request now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::commands) enum Standing {
    /// The engine answered after the failure, or the hold is this run's probe.
    Released,
    /// No answer yet shows the host serves requests; the request is retried once one does.
    AwaitingAnswer,
    /// The host answered and the request failed again, or the host answered unusably.
    NeedsDiagnosis { repeated: bool },
}

impl Standing {
    /// The hold reason a report names, or `None` once the request may be sent again.
    pub(in crate::commands) fn reason(
        self,
        failure: &InvocationFailure,
        retry: &str,
    ) -> Option<String> {
        match self {
            Self::Released => None,
            Self::AwaitingAnswer => Some(format!(
                "{failure}; retried automatically once the {} host answers another request; {retry} retries it now",
                failure.engine().as_str()
            )),
            Self::NeedsDiagnosis { repeated: true } => Some(format!(
                "{failure} again after the host answered; diagnose the request, then use {retry}"
            )),
            Self::NeedsDiagnosis { repeated: false } => {
                Some(format!("{failure}; diagnose the host, then use {retry}"))
            }
        }
    }
}

/// A pending hold that a probe may release.
#[cfg(unix)]
pub(in crate::commands) struct PendingHold {
    pub identity: HoldIdentity,
    /// The queue task whose currency decides whether the probe can reach the request.
    pub task: (String, String),
    pub failed_at: Option<Timestamp>,
}

impl HostAnswers {
    pub(in crate::commands) fn load(root: &Path) -> miette::Result<Self> {
        let path = lk_vault::page_path(root, Path::new(PATH)).into_diagnostic()?;
        let raw = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                std::fs::read(&path).into_diagnostic()?
            }
            Ok(_) => {
                return Err(miette::miette!(
                    "host answers must be a direct regular file"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).into_diagnostic(),
        };
        if raw.len() > MAX_BYTES {
            return Err(miette::miette!("host answers exceed their storage bound"));
        }
        let value = lk_core::json::decode_object(&raw).into_diagnostic()?;
        let stored: Stored = serde_json::from_value(value)
            .map_err(|_| miette::miette!("invalid host answer fields"))?;
        if stored.schema != SCHEMA {
            return Err(miette::miette!("invalid host answer schema"));
        }
        Ok(Self {
            answers: stored.answers,
            probe: stored.probe,
        })
    }

    fn answered_after(&self, engine: AgentEngine, failed_at: Option<Timestamp>) -> bool {
        self.answers
            .get(&engine)
            .is_some_and(|answered| failed_at.is_none_or(|failed| *answered > failed))
    }

    pub(in crate::commands) fn standing(
        &self,
        identity: &HoldIdentity,
        failure: &InvocationFailure,
        exhausted: bool,
    ) -> Standing {
        if !failure.is_unanswered() || exhausted {
            return Standing::NeedsDiagnosis {
                repeated: exhausted,
            };
        }
        let failed_at = failure.failed_at();
        let probed = self
            .probe
            .as_ref()
            .is_some_and(|probe| probe.identity == *identity && probe.failed_at == failed_at);
        if probed || self.answered_after(failure.engine(), failed_at) {
            Standing::Released
        } else {
            Standing::AwaitingAnswer
        }
    }

    /// Whether a new unanswered failure of a request exhausts it: its prior hold was
    /// released by an answer the engine gave after that earlier failure. A probe released
    /// without such an answer renews the hold instead.
    #[cfg(unix)]
    pub(in crate::commands) fn exhausts(
        &self,
        failure: &InvocationFailure,
        prior: Option<(&InvocationFailure, bool)>,
    ) -> bool {
        failure.is_unanswered()
            && prior.is_some_and(|(prior, exhausted)| {
                prior.is_unanswered()
                    && !exhausted
                    && self.answered_after(prior.engine(), prior.failed_at())
            })
    }

    /// The oldest pending hold of a current queue task whose engine has not answered since,
    /// other than a probe released earlier and not yet sent.
    #[cfg(unix)]
    pub(in crate::commands) fn select_probe(
        &self,
        root: &Path,
        holds: Vec<(AgentEngine, PendingHold)>,
    ) -> miette::Result<Option<PendingHold>> {
        let mut candidates = Vec::new();
        for (engine, hold) in holds {
            if self.answered_after(engine, hold.failed_at)
                || self.probe.as_ref().is_some_and(|probe| {
                    probe.identity == hold.identity && probe.failed_at == hold.failed_at
                })
            {
                continue;
            }
            match crate::commands::queue::current_task(root, &hold.task.0) {
                Ok(task) if task.cache_hash == hold.task.1 => candidates.push(hold),
                Ok(_) => {}
                Err(error)
                    if error
                        .downcast_ref::<crate::commands::queue::TaskNotCurrent>()
                        .is_some() => {}
                Err(error) => return Err(error),
            }
        }
        candidates.sort_by(|left, right| {
            left.failed_at.cmp(&right.failed_at).then_with(|| {
                serde_json::to_string(&left.identity)
                    .ok()
                    .cmp(&serde_json::to_string(&right.identity).ok())
            })
        });
        Ok(candidates.into_iter().next())
    }

    #[cfg(unix)]
    fn store(&self, root: &Path) -> miette::Result<()> {
        let path = lk_vault::writable_page_path(root, Path::new(PATH)).into_diagnostic()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).into_diagnostic()?;
        }
        let raw = serde_json::to_vec(&Stored {
            schema: SCHEMA.into(),
            answers: self.answers.clone(),
            probe: self.probe.clone(),
        })
        .into_diagnostic()?;
        lk_core::fs::write_atomic(&path, &raw, None).into_diagnostic()
    }
}

/// The entries of one hold store. A probe scans without holding the vault, so a store or
/// entry an explicit retry or retention removed meanwhile is simply gone.
#[cfg(unix)]
pub(in crate::commands) fn store_entries(
    root: &Path,
    directory: &Path,
) -> miette::Result<Vec<std::path::PathBuf>> {
    let absolute = lk_vault::page_path(root, directory).into_diagnostic()?;
    let gone = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(miette::miette!(
                "hold store {} must be a direct directory",
                directory.display()
            ));
        }
        Err(error) if gone(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error).into_diagnostic(),
    }
    let entries = match std::fs::read_dir(&absolute) {
        Ok(entries) => entries,
        Err(error) if gone(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error).into_diagnostic(),
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(directory.join(entry.file_name())),
            Err(error) if gone(&error) => {}
            Err(error) => return Err(error).into_diagnostic(),
        }
    }
    Ok(paths)
}

/// Every pending unanswered hold of every store, for choosing a probe.
#[cfg(unix)]
pub(in crate::commands) fn pending_holds(
    root: &Path,
) -> miette::Result<Vec<(AgentEngine, PendingHold)>> {
    let mut holds = super::state::pending_inspection_holds(root)?;
    holds.extend(super::generation::rejection::pending_holds(root)?);
    holds.extend(super::selection::rejection::pending_holds(root)?);
    holds.extend(super::preparation::pending_holds(root)?);
    Ok(holds)
}

/// Records that `engine` answered a request when its host ended at `answered_at`. Only a
/// later answer advances the record. Runs hold the workflow lease while they invoke hosts,
/// so these writes do not race.
#[cfg(unix)]
pub(in crate::commands) fn record_answer(
    root: &Path,
    engine: AgentEngine,
    answered_at: Timestamp,
) -> miette::Result<()> {
    let mut answers = HostAnswers::load(root)?;
    if answers
        .answers
        .get(&engine)
        .is_some_and(|answered| *answered >= answered_at)
    {
        return Ok(());
    }
    answers.answers.insert(engine, answered_at);
    answers.store(root)
}

/// Releases `hold` as this vault's probe, replacing any earlier probe.
#[cfg(unix)]
pub(in crate::commands) fn record_probe(root: &Path, hold: &PendingHold) -> miette::Result<()> {
    let mut answers = HostAnswers::load(root)?;
    answers.probe = Some(Probe {
        identity: hold.identity.clone(),
        failed_at: hold.failed_at,
    });
    answers.store(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exited(failed_at: Option<Timestamp>) -> InvocationFailure {
        InvocationFailure::Exited {
            engine: AgentEngine::Codex,
            code: 1,
            failed_at,
        }
    }

    fn identity() -> HoldIdentity {
        HoldIdentity::Generation {
            task_id: "task".into(),
            cache_hash: "a".repeat(32),
        }
    }

    fn at(seconds: i64) -> Option<Timestamp> {
        Some(Timestamp::from_second(seconds).unwrap())
    }

    #[test]
    fn an_unanswered_hold_waits_for_a_later_answer_of_its_engine() {
        let mut answers = HostAnswers::default();
        let standing =
            |answers: &HostAnswers| answers.standing(&identity(), &exited(at(10)), false);
        assert_eq!(standing(&answers), Standing::AwaitingAnswer);
        answers
            .answers
            .insert(AgentEngine::ClaudeCode, at(20).unwrap());
        assert_eq!(standing(&answers), Standing::AwaitingAnswer);
        answers.answers.insert(AgentEngine::Codex, at(10).unwrap());
        assert_eq!(standing(&answers), Standing::AwaitingAnswer);
        answers.answers.insert(AgentEngine::Codex, at(11).unwrap());
        assert_eq!(standing(&answers), Standing::Released);
        assert_eq!(
            answers.standing(&identity(), &exited(None), false),
            Standing::Released,
            "a hold recorded before failure times were kept is released by any answer"
        );
        assert_eq!(
            answers.standing(&identity(), &exited(at(10)), true),
            Standing::NeedsDiagnosis { repeated: true }
        );
        let unusable = InvocationFailure::InvalidResponse {
            engine: AgentEngine::Codex,
            revision: "a".repeat(64),
            bytes: 2,
            failed_at: at(10),
        };
        assert_eq!(
            answers.standing(&identity(), &unusable, false),
            Standing::NeedsDiagnosis { repeated: false }
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_a_failure_after_an_answered_release_exhausts_and_a_probe_renews() {
        let mut answers = HostAnswers::default();
        let prior = exited(at(10));
        let repeat = exited(at(40));
        assert!(!answers.exhausts(&repeat, None));
        assert!(!answers.exhausts(&repeat, Some((&prior, false))));
        answers.probe = Some(Probe {
            identity: identity(),
            failed_at: at(10),
        });
        assert_eq!(
            answers.standing(&identity(), &prior, false),
            Standing::Released
        );
        assert_eq!(
            answers.standing(&identity(), &exited(at(30)), false),
            Standing::AwaitingAnswer,
            "a renewed failure is no longer the probe"
        );
        assert!(!answers.exhausts(&repeat, Some((&prior, false))));
        answers.answers.insert(AgentEngine::Codex, at(20).unwrap());
        assert!(answers.exhausts(&repeat, Some((&prior, false))));
        assert!(!answers.exhausts(&repeat, Some((&exited(at(30)), false))));
    }

    #[cfg(unix)]
    #[test]
    fn a_hold_store_removed_during_a_scan_is_empty_but_an_aliased_one_refuses() {
        let root = tempfile::tempdir().unwrap();
        let store = Path::new(".lorekeeper/queue/generation-rejections");
        assert!(store_entries(root.path(), store).unwrap().is_empty());
        std::fs::create_dir_all(root.path().join(store)).unwrap();
        std::fs::write(root.path().join(store).join("receipt.json"), "{}").unwrap();
        assert_eq!(
            store_entries(root.path(), store).unwrap(),
            [store.join("receipt.json")]
        );
        let elsewhere = root.path().join("elsewhere");
        std::fs::rename(root.path().join(store), &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.path().join(store)).unwrap();
        assert!(store_entries(root.path(), store).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn answers_persist_monotonically_and_refuse_unknown_state() {
        let root = tempfile::tempdir().unwrap();
        assert!(HostAnswers::load(root.path()).unwrap().answers.is_empty());
        record_answer(root.path(), AgentEngine::Codex, at(20).unwrap()).unwrap();
        record_answer(root.path(), AgentEngine::Codex, at(10).unwrap()).unwrap();
        assert_eq!(
            HostAnswers::load(root.path()).unwrap().answers[&AgentEngine::Codex],
            at(20).unwrap(),
            "an answer settled later but given earlier never moves the record back"
        );
        let path = root.path().join(PATH);
        std::fs::write(
            &path,
            br#"{"schema":"host-answers-v1","answers":{},"extra":1}"#,
        )
        .unwrap();
        assert!(HostAnswers::load(root.path()).is_err());
        std::fs::write(&path, br#"{"schema":"other","answers":{}}"#).unwrap();
        assert!(HostAnswers::load(root.path()).is_err());
    }
}
