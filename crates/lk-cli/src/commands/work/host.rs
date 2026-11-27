//! Whether a request whose invocation ended without an answer is still held.
//!
//! An unanswered failure (an exit, a timeout or a missing result) cannot tell a host fault
//! from a request the host cannot serve. Its hold therefore lasts only until the host
//! configuration that served it (its execution profile) answers a later request: that answer
//! shows the host serves requests, so the held request is retried once. Failing again after
//! that answer exhausts it until an explicit retry. While no answer comes, a run with no
//! admissible work releases the oldest pending hold as the probe an operator would send; a
//! probe that fails again only renews its hold. A hold recorded before its host was kept
//! follows any answer of its engine.

use std::collections::BTreeMap;
use std::path::Path;

use jiff::Timestamp;
use lk_core::execution::AgentEngine;
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

use super::state::{HostEnd, InvocationFailure};

const PATH: &str = ".lorekeeper/queue/host-answers.json";
const SCHEMA: &str = "host-answers-v1";
/// The ledger keeps an answer per engine, the latest answers of at most `MAX_PROFILES`
/// profiles under fixed-length keys (about 100 bytes each), and one probe, whose identity
/// names at most a vault path. Whatever configurations share the vault, that stays well inside
/// this bound; a ledger beyond it is refused before it is written, never left for later reads
/// to reject.
const MAX_BYTES: usize = 65_536;
/// The profiles whose latest answer the ledger keeps, most recent first. Several
/// configurations may share one vault, so no run's configuration decides which to keep.
const MAX_PROFILES: usize = 256;

/// A profile name as the ledger keys it: a fixed-length digest, so the ledger's size never
/// depends on how long a configuration names its profiles.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(in crate::commands) struct ProfileKey(String);

impl ProfileKey {
    pub(in crate::commands) fn from_profile(profile: &str) -> Self {
        Self(lk_core::rss_original::digest(profile.as_bytes()))
    }
}

impl TryFrom<String> for ProfileKey {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if lk_core::rss_original::is_valid_digest(&value) {
            Ok(Self(value))
        } else {
            Err("a host answer profile key must be a digest".into())
        }
    }
}

impl From<ProfileKey> for String {
    fn from(key: ProfileKey) -> Self {
        key.0
    }
}

/// The host a failure ended on, as the ledger compares it: its profile's key and the time.
fn host_key(host: Option<&HostEnd>) -> Option<(ProfileKey, Timestamp)> {
    host.map(|host| (ProfileKey::from_profile(&host.profile), host.at))
}

/// The host and time of the failure a probe released.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeHost {
    profile: ProfileKey,
    at: Timestamp,
}

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
    /// `None` for a legacy hold that kept no engine, released by any answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engine: Option<AgentEngine>,
    /// The host and time of the failure released, so a renewed failure is no longer the probe.
    host: Option<ProbeHost>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: String,
    answers: BTreeMap<AgentEngine, Timestamp>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profiles: BTreeMap<ProfileKey, Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probe: Option<Probe>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    recovery_due: std::collections::BTreeSet<ProbeScope>,
}

/// The latest answer each host configuration and each engine gave, the one pending hold
/// released as a probe, and the drains whose next run owes recovery the first turn.
#[derive(Debug, Default)]
pub(in crate::commands) struct HostAnswers {
    answers: BTreeMap<AgentEngine, Timestamp>,
    profiles: BTreeMap<ProfileKey, Timestamp>,
    probe: Option<Probe>,
    #[cfg(unix)]
    recovery_due: std::collections::BTreeSet<ProbeScope>,
}

/// The hosts a run has invoked: the profiles it reached, and their engines for holds that
/// kept no profile.
#[cfg(unix)]
#[derive(Debug, Default)]
pub(in crate::commands) struct InvokedHosts {
    engines: std::collections::BTreeSet<AgentEngine>,
    profiles: std::collections::BTreeSet<ProfileKey>,
}

#[cfg(unix)]
impl InvokedHosts {
    pub(in crate::commands) fn insert(&mut self, engine: AgentEngine, profile: &str) {
        self.engines.insert(engine);
        self.profiles.insert(ProfileKey::from_profile(profile));
    }

    /// Whether this run already invoked the host a hold failed on.
    pub(in crate::commands) fn reached(
        &self,
        engine: Option<AgentEngine>,
        host: Option<&HostEnd>,
    ) -> bool {
        self.reached_profile(engine, host_key(host).map(|(key, _)| key).as_ref())
    }

    /// Whether this run already invoked the host of `profile`, else of `engine`, else (a
    /// legacy hold that kept neither) any host.
    pub(in crate::commands) fn reached_profile(
        &self,
        engine: Option<AgentEngine>,
        profile: Option<&ProfileKey>,
    ) -> bool {
        match (profile, engine) {
            (Some(profile), _) => self.profiles.contains(profile),
            (None, Some(engine)) => self.engines.contains(&engine),
            (None, None) => !self.engines.is_empty(),
        }
    }
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
                "{failure}; retried automatically once the {} host answers another request{}; {retry} retries it now",
                failure
                    .host()
                    .map_or(failure.engine().as_str(), |host| host.profile.as_str()),
                next_probe(failure.host()).map_or_else(String::new, |next| format!(
                    ", or probed again no earlier than {next} after {} failed probe(s)",
                    failure.host().map_or(0, |host| host.probe_failures)
                ))
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

/// How long a hold whose last `probe_failures` consecutive probes ended without an answer
/// waits before it may be probed again: five minutes, doubling per further failed probe, at
/// most a day. A request whose wrapper always fails after the provider served it would
/// otherwise spend one call every scheduled run, indefinitely.
pub(in crate::commands) fn probe_backoff(probe_failures: u32) -> jiff::SignedDuration {
    const FIRST_MINUTES: i64 = 5;
    const MAX_MINUTES: i64 = 24 * 60;
    let doublings = probe_failures.saturating_sub(1).min(16);
    jiff::SignedDuration::from_mins((FIRST_MINUTES << doublings).min(MAX_MINUTES))
}

/// When a hold that failed on `host` may next be probed, if a failed probe spaced it.
pub(in crate::commands) fn next_probe(host: Option<&HostEnd>) -> Option<Timestamp> {
    host.filter(|host| host.probe_failures > 0)
        .and_then(|host| host.at.checked_add(probe_backoff(host.probe_failures)).ok())
}

/// What a pending hold's request belongs to, which decides whether a probe can still reach it.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::commands) enum HoldScope {
    /// A current queue task.
    Queue { task_id: String, cache_hash: String },
    /// An audit subject at the revision the held request read.
    Audit {
        path: String,
        subject_revision: String,
    },
}

#[cfg(unix)]
impl HoldScope {
    /// The drain that reaches this hold.
    pub(in crate::commands) fn drain(&self) -> ProbeScope {
        match self {
            Self::Queue { .. } => ProbeScope::Queue,
            Self::Audit { .. } => ProbeScope::Audit,
        }
    }

    /// Whether the held request still applies. `queue` is the pending queue one probe
    /// selection loaded, read at most once for all of its holds.
    fn is_current(
        &self,
        root: &Path,
        queue: &mut Option<crate::commands::queue::PendingTasks>,
        read: &lk_vault::ReadControl,
    ) -> miette::Result<bool> {
        read.check().into_diagnostic()?;
        match self {
            Self::Queue {
                task_id,
                cache_hash,
            } => {
                let pending = match queue {
                    Some(pending) => pending,
                    None => queue.insert(crate::commands::queue::PendingTasks::load_controlled(
                        root, read,
                    )?),
                };
                match pending.current_controlled(root, task_id, read) {
                    Ok(task) => Ok(task.cache_hash == *cache_hash),
                    Err(error)
                        if error
                            .downcast_ref::<crate::commands::queue::TaskNotCurrent>()
                            .is_some() =>
                    {
                        Ok(false)
                    }
                    Err(error) => Err(error),
                }
            }
            Self::Audit {
                path,
                subject_revision,
            } => {
                let absolute = lk_vault::page_path(root, Path::new(path)).into_diagnostic()?;
                match std::fs::read_to_string(absolute) {
                    Ok(text) => Ok(lk_core::concept::file_revision(&text) == *subject_revision),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error).into_diagnostic(),
                }
            }
        }
    }
}

/// Which pending holds an operation can probe: the queue drain reaches queue tasks, the
/// knowledge audit reaches audit subjects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(in crate::commands) enum ProbeScope {
    Queue,
    Audit,
}

/// A pending hold that a probe may release.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub(in crate::commands) struct PendingHold {
    pub identity: HoldIdentity,
    /// The engine that failed; `None` for a legacy timeout that kept none.
    pub engine: Option<AgentEngine>,
    pub scope: HoldScope,
    /// The host that ended the failed request, if it was recorded.
    pub host: Option<HostEnd>,
}

impl PendingHold {
    /// Whether `other` is this hold as one capture of the pending holds listed it: the same
    /// held input, failed when the same host ended. A renewed failure is a different hold.
    pub(in crate::commands) fn is_same_hold(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.host.as_ref().map(|host| host.at) == other.host.as_ref().map(|host| host.at)
    }
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
            profiles: stored.profiles,
            probe: stored.probe,
            #[cfg(unix)]
            recovery_due: stored.recovery_due,
        })
    }

    /// Whether any engine answered since answers were recorded, which releases a hold that
    /// kept neither its engine nor its time.
    pub(in crate::commands) fn has_answer(&self) -> bool {
        !self.answers.is_empty()
    }

    /// Whether the host a failure was recorded on answered after it ended; a failure that kept
    /// no host follows any answer of its engine, and a legacy one that kept no engine follows
    /// any answer. Admission, exhaustion and probing all match hosts through this.
    fn answered_since(
        &self,
        engine: Option<AgentEngine>,
        host: Option<(ProfileKey, Timestamp)>,
    ) -> bool {
        match (host, engine) {
            (Some((profile, at)), _) => self
                .profiles
                .get(&profile)
                .is_some_and(|answered| *answered > at),
            (None, Some(engine)) => self.answers.contains_key(&engine),
            (None, None) => self.has_answer(),
        }
    }

    /// Whether `identity`, failed at `failed_at`, is the vault's released probe.
    pub(in crate::commands) fn is_probe(
        &self,
        identity: &HoldIdentity,
        failed_at: Option<Timestamp>,
    ) -> bool {
        self.probe.as_ref().is_some_and(|probe| {
            probe.identity == *identity && probe.host.as_ref().map(|host| host.at) == failed_at
        })
    }

    /// Whether a legacy timeout hold, which kept neither engine nor time, still holds: no host
    /// has answered and it is not the released probe.
    pub(in crate::commands) fn holds_legacy(&self, identity: &HoldIdentity) -> bool {
        !self.answered_since(None, None) && !self.is_probe(identity, None)
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
        if self.is_probe(identity, failure.host().map(|host| host.at))
            || self.answered_since(Some(failure.engine()), host_key(failure.host()))
        {
            Standing::Released
        } else {
            Standing::AwaitingAnswer
        }
    }

    /// Settles a new failure of a request against its `prior` hold and returns whether it
    /// exhausts the request: an unanswered failure after an answer of the prior hold's host
    /// released it. A failure while the prior hold still awaited an answer was its probe: it
    /// renews the hold, and `failure` records one more consecutive probe failure, which spaces
    /// the next probe (`probe_backoff`).
    #[cfg(unix)]
    pub(in crate::commands) fn settle(
        &self,
        failure: &mut InvocationFailure,
        prior: Option<(&InvocationFailure, bool)>,
    ) -> bool {
        let Some((prior, false)) = prior.filter(|(prior, _)| prior.is_unanswered()) else {
            return false;
        };
        let released = self.answered_since(Some(prior.engine()), host_key(prior.host()));
        self.settle_renewal(failure, released, prior.host())
    }

    /// [`Self::settle`] after a legacy timeout hold, which kept neither engine nor time and
    /// is released by any answer.
    #[cfg(unix)]
    pub(in crate::commands) fn settle_legacy(&self, failure: &mut InvocationFailure) -> bool {
        let released = self.answered_since(None, None);
        self.settle_renewal(failure, released, None)
    }

    #[cfg(unix)]
    fn settle_renewal(
        &self,
        failure: &mut InvocationFailure,
        released: bool,
        prior: Option<&HostEnd>,
    ) -> bool {
        if !failure.is_unanswered() {
            return false;
        }
        if released {
            return true;
        }
        let renewed = prior
            .map_or(0, |host| host.probe_failures)
            .saturating_add(1);
        if let Some(host) = failure.host_mut() {
            host.probe_failures = renewed;
        }
        false
    }

    /// The pending hold an earlier run released as this vault's probe, while it still stands
    /// released: the same request, failed at the same time (neither settled nor renewed since),
    /// whose host has not answered since. That run may have ended before sending it, so this
    /// run takes it as its drain's released probe; without such a hold the ledger's probe is
    /// stale.
    #[cfg(unix)]
    pub(in crate::commands) fn inherited_probe(
        &self,
        holds: Vec<PendingHold>,
    ) -> Option<PendingHold> {
        holds.into_iter().find(|hold| {
            self.is_probe(&hold.identity, hold.host.as_ref().map(|host| host.at))
                && !self.answered_since(hold.engine, host_key(hold.host.as_ref()))
        })
    }

    /// Whether the ledger names a released probe.
    #[cfg(unix)]
    pub(in crate::commands) fn has_probe(&self) -> bool {
        self.probe.is_some()
    }

    /// The pending holds a probe may release for `scope`, oldest failure first: their
    /// request is still current, their probe backoff has elapsed by `now`, their host has
    /// not answered since, and this run has not yet
    /// invoked that host.
    #[cfg(unix)]
    pub(in crate::commands) fn probe_candidates(
        &self,
        root: &Path,
        scope: ProbeScope,
        invoked: &InvokedHosts,
        holds: Vec<PendingHold>,
        read: &lk_vault::ReadControl,
        now: Timestamp,
    ) -> miette::Result<Vec<PendingHold>> {
        let mut candidates = Vec::new();
        let mut queue = None;
        for hold in holds {
            let reachable = matches!(
                (&hold.scope, scope),
                (HoldScope::Queue { .. }, ProbeScope::Queue)
                    | (HoldScope::Audit { .. }, ProbeScope::Audit)
            );
            if reachable
                && !invoked.reached(hold.engine, hold.host.as_ref())
                && !self.answered_since(hold.engine, host_key(hold.host.as_ref()))
                && next_probe(hold.host.as_ref()).is_none_or(|next| next <= now)
                && hold.scope.is_current(root, &mut queue, read)?
            {
                candidates.push(hold);
            }
        }
        candidates.sort_by(|left, right| {
            let failed_at = |hold: &PendingHold| hold.host.as_ref().map(|host| host.at);
            failed_at(left).cmp(&failed_at(right)).then_with(|| {
                serde_json::to_string(&left.identity)
                    .ok()
                    .cmp(&serde_json::to_string(&right.identity).ok())
            })
        });
        Ok(candidates)
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
            profiles: self.profiles.clone(),
            probe: self.probe.clone(),
            recovery_due: self.recovery_due.clone(),
        })
        .into_diagnostic()?;
        if raw.len() > MAX_BYTES {
            return Err(miette::miette!(
                "host answers for {} profiles exceed their storage bound",
                self.profiles.len()
            ));
        }
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
pub(in crate::commands) fn pending_holds(root: &Path) -> miette::Result<Vec<PendingHold>> {
    let mut holds = super::state::pending_inspection_holds(root)?;
    holds.extend(super::generation::rejection::pending_holds(root)?);
    holds.extend(super::selection::rejection::pending_holds(root)?);
    holds.extend(super::preparation::pending_holds(root)?);
    Ok(holds)
}

/// Records that the host configuration `profile` of `engine` answered a request when it
/// ended at `answered_at`. Only a later answer advances either record, and the ledger keeps
/// the `MAX_PROFILES` most recently answering profiles, so renamed or removed profiles age
/// out without any configuration deciding for another sharing the vault. Runs hold the
/// workflow lease while they invoke hosts, so these writes do not race.
#[cfg(unix)]
pub(in crate::commands) fn record_answer(
    root: &Path,
    engine: AgentEngine,
    profile: &str,
    answered_at: Timestamp,
) -> miette::Result<()> {
    fn advance<K: Ord>(answers: &mut BTreeMap<K, Timestamp>, key: K, at: Timestamp) -> bool {
        if answers.get(&key).is_some_and(|answered| *answered >= at) {
            return false;
        }
        answers.insert(key, at);
        true
    }
    let mut answers = HostAnswers::load(root)?;
    let engine_advanced = advance(&mut answers.answers, engine, answered_at);
    let profile_advanced = advance(
        &mut answers.profiles,
        ProfileKey::from_profile(profile),
        answered_at,
    );
    while answers.profiles.len() > MAX_PROFILES {
        let oldest = answers
            .profiles
            .iter()
            .min_by_key(|(key, answered)| (**answered, (*key).clone()))
            .map(|(key, _)| key.clone())
            .expect("profiles beyond the bound");
        answers.profiles.remove(&oldest);
    }
    if engine_advanced || profile_advanced {
        answers.store(root)?;
    }
    Ok(())
}

/// Records whether the next run of `scope`'s drain owes recovery its first turn: a run that
/// gave its turn to ordinary work while probe candidates waited owes it; a release pays it.
/// Alternating keeps either from taking every turn, even at one call per run.
#[cfg(unix)]
pub(in crate::commands) fn record_recovery_due(
    root: &Path,
    scope: ProbeScope,
    due: bool,
) -> miette::Result<()> {
    let mut answers = HostAnswers::load(root)?;
    let changed = if due {
        answers.recovery_due.insert(scope)
    } else {
        answers.recovery_due.remove(&scope)
    };
    if changed {
        answers.store(root)?;
    }
    Ok(())
}

impl HostAnswers {
    /// Whether `scope`'s drain owes recovery its first turn in this run.
    #[cfg(unix)]
    pub(in crate::commands) fn is_recovery_due(&self, scope: ProbeScope) -> bool {
        self.recovery_due.contains(&scope)
    }
}

/// Releases `hold` as this vault's probe, replacing any earlier probe, or withdraws the
/// probe when `hold` is `None`.
#[cfg(unix)]
pub(in crate::commands) fn record_probe(
    root: &Path,
    hold: Option<&PendingHold>,
) -> miette::Result<()> {
    let mut answers = HostAnswers::load(root)?;
    let probe = hold.map(|hold| Probe {
        identity: hold.identity.clone(),
        engine: hold.engine,
        host: hold.host.as_ref().map(|host| ProbeHost {
            profile: ProfileKey::from_profile(&host.profile),
            at: host.at,
        }),
    });
    if answers.probe == probe {
        return Ok(());
    }
    answers.probe = probe;
    answers.store(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Option<Timestamp> {
        Some(Timestamp::from_second(seconds).unwrap())
    }

    fn host(profile: &str, seconds: i64) -> Option<HostEnd> {
        Some(HostEnd {
            profile: profile.into(),
            at: at(seconds).unwrap(),
            probe_failures: 0,
        })
    }

    fn exited(host: Option<HostEnd>) -> InvocationFailure {
        InvocationFailure::Exited {
            engine: AgentEngine::Codex,
            code: 1,
            host,
        }
    }

    fn identity() -> HoldIdentity {
        HoldIdentity::Generation {
            task_id: "task".into(),
            cache_hash: "a".repeat(32),
        }
    }

    #[test]
    fn an_unanswered_hold_waits_for_a_later_answer_of_its_own_host() {
        let mut answers = HostAnswers::default();
        let failed = exited(host("synthesis", 10));
        let standing = |answers: &HostAnswers| answers.standing(&identity(), &failed, false);
        assert_eq!(standing(&answers), Standing::AwaitingAnswer);
        answers.answers.insert(AgentEngine::Codex, at(20).unwrap());
        answers
            .profiles
            .insert(ProfileKey::from_profile("inspection"), at(20).unwrap());
        assert_eq!(
            standing(&answers),
            Standing::AwaitingAnswer,
            "another profile of the same engine is another host"
        );
        answers
            .profiles
            .insert(ProfileKey::from_profile("synthesis"), at(10).unwrap());
        assert_eq!(standing(&answers), Standing::AwaitingAnswer);
        answers
            .profiles
            .insert(ProfileKey::from_profile("synthesis"), at(11).unwrap());
        assert_eq!(standing(&answers), Standing::Released);
        assert!(
            Standing::AwaitingAnswer
                .reason(&failed, "retry")
                .unwrap()
                .contains("once the synthesis host answers")
        );
        assert_eq!(
            answers.standing(&identity(), &exited(None), false),
            Standing::Released,
            "a hold that kept no host follows any answer of its engine"
        );
        assert_eq!(
            answers.standing(&identity(), &failed, true),
            Standing::NeedsDiagnosis { repeated: true }
        );
        let unusable = InvocationFailure::InvalidResponse {
            engine: AgentEngine::Codex,
            revision: "a".repeat(64),
            bytes: 2,
            host: host("synthesis", 10),
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
        let prior = exited(host("synthesis", 10));
        let repeat = exited(host("synthesis", 40));
        assert!(!answers.settle(&mut repeat.clone(), None));
        assert!(!answers.settle(&mut repeat.clone(), Some((&prior, false))));
        answers.probe = Some(Probe {
            identity: identity(),
            engine: Some(AgentEngine::Codex),
            host: Some(ProbeHost {
                profile: ProfileKey::from_profile("synthesis"),
                at: at(10).unwrap(),
            }),
        });
        let pending = |seconds| {
            vec![PendingHold {
                identity: identity(),
                engine: Some(AgentEngine::Codex),
                scope: HoldScope::Audit {
                    path: "wiki/concepts/subject.md".into(),
                    subject_revision: "0".repeat(32),
                },
                host: host("synthesis", seconds),
            }]
        };
        assert_eq!(
            answers
                .inherited_probe(pending(10))
                .map(|hold| hold.identity),
            Some(identity()),
            "an unanswered released probe carries into the next run"
        );
        assert!(
            answers.inherited_probe(pending(30)).is_none(),
            "a renewed hold no longer carries its old release"
        );
        assert_eq!(
            answers.standing(&identity(), &prior, false),
            Standing::Released
        );
        assert_eq!(
            answers.standing(&identity(), &exited(host("synthesis", 30)), false),
            Standing::AwaitingAnswer,
            "a renewed failure is no longer the probe"
        );
        assert!(!answers.settle(&mut repeat.clone(), Some((&prior, false))));
        answers
            .profiles
            .insert(ProfileKey::from_profile("inspection"), at(20).unwrap());
        assert!(
            !answers.settle(&mut repeat.clone(), Some((&prior, false))),
            "another host's answer released nothing"
        );
        answers
            .profiles
            .insert(ProfileKey::from_profile("synthesis"), at(20).unwrap());
        assert!(
            answers.inherited_probe(pending(10)).is_none(),
            "an answer of the probe's host settles it"
        );
        assert!(answers.settle(&mut repeat.clone(), Some((&prior, false))));
        assert!(!answers.settle(
            &mut repeat.clone(),
            Some((&exited(host("synthesis", 30)), false))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn each_failed_probe_doubles_the_wait_before_the_next_from_five_minutes_to_a_day() {
        let minutes = |failures| probe_backoff(failures).as_mins();
        assert_eq!(
            [1, 2, 3, 4, 9, 64, u32::MAX].map(minutes),
            [5, 10, 20, 40, 1_280, 1_440, 1_440]
        );
        let answers = HostAnswers::default();
        let mut first = exited(host("synthesis", 10));
        assert!(!answers.settle(&mut first, None));
        assert_eq!(
            first.host().unwrap().probe_failures,
            0,
            "no probe preceded it"
        );
        let mut probed = exited(host("synthesis", 400));
        assert!(!answers.settle(&mut probed, Some((&first, false))));
        assert_eq!(probed.host().unwrap().probe_failures, 1);
        let mut again = exited(host("synthesis", 800));
        assert!(!answers.settle(&mut again, Some((&probed, false))));
        assert_eq!(again.host().unwrap().probe_failures, 2);
        assert_eq!(
            next_probe(again.host()),
            Some(Timestamp::from_second(800 + 10 * 60).unwrap())
        );
        let reason = answers
            .standing(&identity(), &again, false)
            .reason(&again, "lore work retry");
        assert!(
            reason.is_some_and(|reason| reason.contains("probed again no earlier than")),
            "the report names when the hold is next probed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_candidates_are_current_reachable_unreached_due_and_oldest_first() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki/concepts")).unwrap();
        std::fs::write(root.path().join("wiki/concepts/subject.md"), "Subject").unwrap();
        let current = lk_core::concept::file_revision("Subject");
        let hold = |node: &str, profile: &str, subject_revision: &str, seconds| PendingHold {
            identity: HoldIdentity::Inspection {
                work_id: "w".repeat(64),
                node_id: node.into(),
            },
            engine: Some(AgentEngine::Codex),
            scope: HoldScope::Audit {
                path: "wiki/concepts/subject.md".into(),
                subject_revision: subject_revision.into(),
            },
            host: host(profile, seconds),
        };
        let candidates = |answers: &HostAnswers, scope, invoked: &InvokedHosts| {
            let holds = vec![
                hold("newer", "synthesis", &current, 20),
                hold("older", "inspection", &current, 10),
                hold("stale", "synthesis", "changed", 5),
                PendingHold {
                    host: Some(HostEnd {
                        profile: "spaced".into(),
                        at: at(900).unwrap(),
                        probe_failures: 1,
                    }),
                    ..hold("spaced", "spaced", &current, 900)
                },
            ];
            answers
                .probe_candidates(
                    root.path(),
                    scope,
                    invoked,
                    holds,
                    &lk_vault::ReadControl::default(),
                    at(1_000).unwrap(),
                )
                .unwrap()
                .into_iter()
                .map(|hold| match hold.identity {
                    HoldIdentity::Inspection { node_id, .. } => node_id,
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
        };
        let mut answers = HostAnswers::default();
        let none = InvokedHosts::default();
        assert_eq!(
            candidates(&answers, ProbeScope::Audit, &none),
            ["older", "newer"]
        );
        assert!(
            candidates(&answers, ProbeScope::Queue, &none).is_empty(),
            "a queue drain cannot reach an audit subject"
        );
        let mut invoked = InvokedHosts::default();
        invoked.insert(AgentEngine::Codex, "inspection");
        assert_eq!(
            candidates(&answers, ProbeScope::Audit, &invoked),
            ["newer"],
            "a host this run already reached needs no probe; another profile still does"
        );
        answers
            .profiles
            .insert(ProfileKey::from_profile("synthesis"), at(30).unwrap());
        assert_eq!(candidates(&answers, ProbeScope::Audit, &none), ["older"]);
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
    fn the_ledger_keeps_the_most_recent_profiles_within_its_read_bound() {
        let root = tempfile::tempdir().unwrap();
        let long = "p".repeat(10_000);
        let names = (0..MAX_PROFILES + 40)
            .map(|index| format!("{long}-{index:04}"))
            .collect::<Vec<_>>();
        for (index, name) in names.iter().enumerate() {
            record_answer(
                root.path(),
                AgentEngine::Codex,
                name,
                at(100 + index as i64).unwrap(),
            )
            .expect("however long or many the profile names, the ledger stays bounded");
        }
        let raw = std::fs::read(root.path().join(PATH)).unwrap();
        assert!(raw.len() <= MAX_BYTES, "{}", raw.len());
        let loaded = HostAnswers::load(root.path()).unwrap();
        assert_eq!(loaded.profiles.len(), MAX_PROFILES);
        assert!(
            !loaded
                .profiles
                .contains_key(&ProfileKey::from_profile(&names[0])),
            "the least recently answering profile ages out"
        );
        assert!(
            loaded
                .profiles
                .contains_key(&ProfileKey::from_profile(names.last().unwrap())),
            "whichever configuration answered last keeps its evidence"
        );
        std::fs::write(
            root.path().join(PATH),
            br#"{"schema":"host-answers-v1","answers":{},"profiles":{"primary":"2026-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert!(
            HostAnswers::load(root.path()).is_err(),
            "a profile is stored only by its key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn answers_persist_monotonically_and_refuse_unknown_state() {
        let root = tempfile::tempdir().unwrap();
        assert!(HostAnswers::load(root.path()).unwrap().answers.is_empty());
        record_answer(
            root.path(),
            AgentEngine::Codex,
            "synthesis",
            at(20).unwrap(),
        )
        .unwrap();
        record_answer(
            root.path(),
            AgentEngine::Codex,
            "synthesis",
            at(10).unwrap(),
        )
        .unwrap();
        let loaded = HostAnswers::load(root.path()).unwrap();
        assert_eq!(
            loaded.profiles[&ProfileKey::from_profile("synthesis")],
            at(20).unwrap(),
            "an answer settled later but given earlier never moves the record back"
        );
        assert_eq!(loaded.answers[&AgentEngine::Codex], at(20).unwrap());
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
