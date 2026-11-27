use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::civil::Date;
use lk_core::execution::{AutomationConfig, ExecutionOperation};
use miette::IntoDiagnostic;
use serde::{Deserialize, Serialize};

const USAGE_PATH: &str = ".lorekeeper/automation-usage.json";
const MAX_USAGE_BYTES: usize = 16_384;
/// The job report keeps the complete detail; the ledger keeps a prefix so that a pause
/// for every operation always fits `MAX_USAGE_BYTES`.
const MAX_HALT_DETAIL_BYTES: usize = 1024;

#[derive(Clone, Copy, Serialize)]
enum Schema {
    #[serde(rename = "automation-usage-v2")]
    V2,
}

#[derive(Deserialize)]
#[serde(tag = "schema", deny_unknown_fields)]
enum StoredUsage {
    #[serde(rename = "automation-usage-v1")]
    V1 {
        utc_date: Date,
        reserved_calls: usize,
        paused: BTreeMap<ExecutionOperation, jiff::Timestamp>,
    },
    #[serde(rename = "automation-usage-v2")]
    V2 {
        utc_date: Date,
        reserved_calls: usize,
        paused: BTreeMap<ExecutionOperation, Pause>,
    },
}

#[derive(Serialize)]
struct Usage {
    schema: Schema,
    utc_date: Date,
    reserved_calls: usize,
    paused: BTreeMap<ExecutionOperation, Pause>,
}

impl From<StoredUsage> for Usage {
    fn from(stored: StoredUsage) -> Self {
        let (utc_date, reserved_calls, paused) = match stored {
            StoredUsage::V1 {
                utc_date,
                reserved_calls,
                paused,
            } => (
                utc_date,
                reserved_calls,
                paused
                    .into_iter()
                    .map(|(operation, at)| (operation, Pause { at, cause: None }))
                    .collect(),
            ),
            StoredUsage::V2 {
                utc_date,
                reserved_calls,
                paused,
            } => (utc_date, reserved_calls, paused),
        };
        Self {
            schema: Schema::V2,
            utc_date,
            reserved_calls,
            paused,
        }
    }
}

/// The stage a run records when a profile's host cannot start (`execution::preflight_hosts`,
/// or its first call). It never halts: the condition is current, and each run checks again.
pub(super) const HOST_ENVIRONMENT_STAGE: &str = "host environment";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct Pause {
    pub at: jiff::Timestamp,
    /// `None` only for a pause recorded by the v1 ledger, which kept no cause.
    pub cause: Option<HaltCause>,
}

/// The failed stage that stopped a native job, as named in its report.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::commands) struct HaltCause {
    pub stage: String,
    pub detail: Option<String>,
}

impl HaltCause {
    pub fn new(stage: &str, detail: Option<&str>) -> Self {
        Self {
            stage: stage.into(),
            detail: detail.map(|detail| {
                if detail.len() <= MAX_HALT_DETAIL_BYTES {
                    return detail.to_owned();
                }
                let mut end = MAX_HALT_DETAIL_BYTES - '…'.len_utf8();
                while !detail.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &detail[..end])
            }),
        }
    }
}

fn path(root: &Path) -> miette::Result<PathBuf> {
    lk_vault::page_path(root, Path::new(USAGE_PATH)).into_diagnostic()
}

impl Usage {
    fn load(root: &Path, today: Date) -> miette::Result<Self> {
        use rustix::fs::{Mode, OFlags};
        let descriptor = match rustix::fs::open(
            path(root)?,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(descriptor) => descriptor,
            Err(rustix::io::Errno::NOENT) => {
                return Ok(Self {
                    schema: Schema::V2,
                    utc_date: today,
                    reserved_calls: 0,
                    paused: BTreeMap::new(),
                });
            }
            Err(error) => return Err(error).into_diagnostic(),
        };
        let file = std::fs::File::from(descriptor);
        if !file.metadata().into_diagnostic()?.is_file() {
            return Err(miette::miette!(
                "automation usage must be a direct regular file"
            ));
        }
        let mut bytes = Vec::new();
        file.take((MAX_USAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .into_diagnostic()?;
        if bytes.len() > MAX_USAGE_BYTES {
            return Err(miette::miette!(
                "automation usage exceeds its storage bound"
            ));
        }
        let value = lk_core::json::decode_object(&bytes).map_err(|_| {
            miette::miette!("automation usage requires a JSON object with unique fields")
        })?;
        let mut usage = Self::from(
            serde_json::from_value::<StoredUsage>(value)
                .map_err(|_| miette::miette!("invalid automation usage schema or fields"))?,
        );
        // 0.163 paused an operation when a profile's host environment could not start. That
        // stage no longer halts, so such a pause holds nothing an operator must repair.
        usage.paused.retain(|_, pause| {
            pause
                .cause
                .as_ref()
                .is_none_or(|cause| cause.stage != HOST_ENVIRONMENT_STAGE)
        });
        if usage.utc_date > today {
            return Err(miette::miette!("automation UTC date moved backwards"));
        }
        if usage.utc_date < today {
            usage.utc_date = today;
            usage.reserved_calls = 0;
        }
        Ok(usage)
    }

    fn save(&self, root: &Path) -> miette::Result<()> {
        let bytes = serde_json::to_vec(self).into_diagnostic()?;
        lk_core::fs::write_atomic(&path(root)?, &bytes, Some(0o600)).into_diagnostic()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Stop {
    DayLimit,
    RunLimit,
    RunDuration,
    WaitDuration,
    Paused,
}

#[derive(Clone, Serialize)]
pub(super) struct Report {
    pub utc_date: Option<Date>,
    pub reserved_calls: Option<usize>,
    pub max_calls_per_day: Option<usize>,
    pub max_calls_per_run: usize,
    pub run_reservations: usize,
    pub workflow_wait_ms: u64,
    pub stop: Option<Stop>,
}

/// The native Runner serializes this operational ledger under its acquired
/// physical-vault workflow lease. Synchronous atomic reservations precede host
/// launch; they are independent of canonical mutation journals and are never refunded.
pub(super) struct Control {
    policy: AutomationConfig,
    operation: ExecutionOperation,
    resume: bool,
    deadline: Option<Instant>,
    wait_remaining: Duration,
    turn_started: Option<Instant>,
    pub report: Report,
}

fn today() -> Date {
    jiff::Timestamp::now()
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .date()
}

#[derive(Serialize)]
pub(in crate::commands) struct Status {
    pub utc_date: Date,
    pub reserved_calls: usize,
    pub max_calls_per_day: Option<usize>,
    pub remaining_calls: Option<usize>,
    pub max_run_seconds: u64,
    pub max_wait_seconds: u64,
    pub yield_after_seconds: u64,
    pub paused: BTreeMap<ExecutionOperation, Pause>,
}

pub(in crate::commands) fn status(
    root: &Path,
    policy: &AutomationConfig,
) -> miette::Result<Status> {
    let usage = Usage::load(root, today())?;
    Ok(Status {
        utc_date: usage.utc_date,
        reserved_calls: usage.reserved_calls,
        max_calls_per_day: policy.max_calls_per_day.map(|limit| limit.get()),
        remaining_calls: policy
            .max_calls_per_day
            .map(|limit| limit.get().saturating_sub(usage.reserved_calls)),
        max_run_seconds: policy.max_run_seconds.get(),
        max_wait_seconds: policy.wait_duration().as_secs(),
        yield_after_seconds: policy.yield_after_seconds,
        paused: usage.paused,
    })
}

impl Control {
    pub fn new(
        policy: AutomationConfig,
        operation: ExecutionOperation,
        resume: bool,
        max_calls_per_run: usize,
    ) -> miette::Result<Self> {
        let wait_remaining = policy.wait_duration();
        Instant::now()
            .checked_add(Duration::from_secs(policy.max_run_seconds.get()))
            .ok_or_else(|| miette::miette!("automation duration exceeds the monotonic clock"))?;
        Instant::now().checked_add(wait_remaining).ok_or_else(|| {
            miette::miette!("automation waiting duration exceeds the monotonic clock")
        })?;
        Ok(Self {
            report: Report {
                utc_date: None,
                reserved_calls: None,
                max_calls_per_day: policy.max_calls_per_day.map(|limit| limit.get()),
                max_calls_per_run,
                run_reservations: 0,
                workflow_wait_ms: 0,
                stop: None,
            },
            policy,
            operation,
            resume,
            deadline: None,
            wait_remaining,
            turn_started: None,
        })
    }

    pub fn preflight(&mut self, root: &Path) -> miette::Result<bool> {
        if self.resume || !root.try_exists().into_diagnostic()? {
            return Ok(true);
        }
        let pin = crate::commands::workflow::VaultRootPin::capture(root, root)?;
        let usage = Usage::load(pin.root(), today())?;
        pin.validate()?;
        if usage.paused.contains_key(&self.operation) {
            self.refresh(&usage);
            self.report.stop = Some(Stop::Paused);
            return Ok(false);
        }
        Ok(true)
    }

    pub fn wait_deadline(&mut self) -> miette::Result<Option<Instant>> {
        if !self.check_duration() {
            return Ok(None);
        }
        if self.wait_remaining.is_zero() {
            self.report.stop = Some(Stop::WaitDuration);
            return Ok(None);
        }
        Instant::now()
            .checked_add(self.wait_remaining)
            .map(Some)
            .ok_or_else(|| {
                miette::miette!("automation waiting duration exceeds the monotonic clock")
            })
    }

    pub fn record_wait(&mut self, elapsed: Duration) -> miette::Result<()> {
        if elapsed >= self.wait_remaining {
            self.report.stop = Some(Stop::WaitDuration);
        }
        self.wait_remaining = self.wait_remaining.saturating_sub(elapsed);
        self.report.workflow_wait_ms = self
            .report
            .workflow_wait_ms
            .saturating_add(elapsed.as_millis().try_into().unwrap_or(u64::MAX));
        if let Some(deadline) = self.deadline {
            self.deadline = Some(deadline.checked_add(elapsed).ok_or_else(|| {
                miette::miette!("automation duration exceeds the monotonic clock")
            })?);
        }
        Ok(())
    }

    pub fn stop_waiting(&mut self) {
        self.report.stop = Some(Stop::WaitDuration);
    }

    pub fn available(&self) -> usize {
        let reserved = self
            .report
            .reserved_calls
            .expect("usage read under owned workflow");
        let run_remaining = self
            .report
            .max_calls_per_run
            .saturating_sub(self.report.run_reservations);
        self.policy
            .max_calls_per_day
            .map_or(run_remaining, |limit| {
                limit.get().saturating_sub(reserved).min(run_remaining)
            })
    }

    pub fn has_remaining_budget(&self) -> bool {
        self.report.stop.is_none()
            && !self.wait_remaining.is_zero()
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() < deadline)
            && self.available() != 0
    }

    /// Whether the current workflow turn has lasted long enough to yield to a waiter.
    pub fn may_yield(&self) -> bool {
        self.turn_started
            .is_some_and(|started| started.elapsed() >= self.policy.yield_after())
    }

    pub fn check_duration(&mut self) -> bool {
        if self.report.stop.is_none()
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.report.stop = Some(Stop::RunDuration);
        }
        self.report.stop.is_none()
    }

    /// Starts a workflow turn: called once each time the run acquires the workflow.
    pub fn enter(&mut self, root: &Path) -> miette::Result<bool> {
        if !self.check_duration() {
            return Ok(false);
        }
        self.turn_started = Some(Instant::now());
        if self.deadline.is_none() {
            self.deadline = Some(
                Instant::now()
                    .checked_add(Duration::from_secs(self.policy.max_run_seconds.get()))
                    .ok_or_else(|| {
                        miette::miette!("automation duration exceeds the monotonic clock")
                    })?,
            );
        }
        let mut usage = Usage::load(root, today())?;
        if self.resume && usage.paused.remove(&self.operation).is_some() {
            usage.save(root)?;
        }
        self.resume = false;
        self.refresh(&usage);
        if usage.paused.contains_key(&self.operation) {
            self.report.stop = Some(Stop::Paused);
        }
        Ok(self.report.stop.is_none())
    }

    fn refresh(&mut self, usage: &Usage) {
        self.report.utc_date = Some(usage.utc_date);
        self.report.reserved_calls = Some(usage.reserved_calls);
    }

    pub fn reserve(&mut self, root: &Path, count: usize) -> miette::Result<bool> {
        if count == 0 {
            return Err(miette::miette!("automation reservations must be positive"));
        }
        if !self.check_duration() {
            return Ok(false);
        }
        let mut usage = Usage::load(root, today())?;
        self.refresh(&usage);
        if usage.paused.contains_key(&self.operation) {
            self.report.stop = Some(Stop::Paused);
            return Ok(false);
        }
        let reserved = usage
            .reserved_calls
            .checked_add(count)
            .ok_or_else(|| miette::miette!("automation invocation counter overflow"))?;
        if self
            .policy
            .max_calls_per_day
            .is_some_and(|limit| reserved > limit.get())
        {
            self.report.stop = Some(Stop::DayLimit);
            return Ok(false);
        }
        let run_reservations = self
            .report
            .run_reservations
            .checked_add(count)
            .ok_or_else(|| miette::miette!("automation run counter overflow"))?;
        if run_reservations > self.report.max_calls_per_run {
            self.report.stop = Some(Stop::RunLimit);
            return Ok(false);
        }
        usage.reserved_calls = reserved;
        usage.save(root)?;
        self.refresh(&usage);
        self.report.run_reservations = run_reservations;
        Ok(self.check_duration())
    }

    pub fn stop_for_limit(&mut self) {
        self.report.stop = Some(
            if self.policy.max_calls_per_day.is_some_and(|limit| {
                self.report.reserved_calls.expect("admitted usage") >= limit.get()
            }) {
                Stop::DayLimit
            } else {
                Stop::RunLimit
            },
        );
    }

    pub fn pause(&mut self, root: &Path, cause: HaltCause) -> miette::Result<()> {
        let mut usage = Usage::load(root, today())?;
        usage.paused.insert(
            self.operation,
            Pause {
                at: jiff::Timestamp::now(),
                cause: Some(cause),
            },
        );
        usage.save(root)?;
        self.refresh(&usage);
        self.report.stop = Some(Stop::Paused);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".lorekeeper")).unwrap();
        directory
    }

    fn policy(limit: usize) -> AutomationConfig {
        AutomationConfig {
            process_schedule: "0,15,30,45 * * * *".into(),
            knowledge_schedule: None,
            max_calls_per_day: Some(limit.try_into().unwrap()),
            max_run_seconds: 60.try_into().unwrap(),
            max_wait_seconds: None,
            yield_after_seconds: 600,
        }
    }

    #[test]
    fn reservations_survive_restart_and_resume_does_not_reset_usage() {
        let directory = fixture();
        let root = directory.path();
        let mut first = Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        assert!(first.enter(root).unwrap());
        assert!(first.reserve(root, 2).unwrap());
        first
            .pause(root, HaltCause::new("native result host", None))
            .unwrap();
        let mut restarted =
            Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        assert!(!restarted.enter(root).unwrap());
        assert_eq!(restarted.report.stop, Some(Stop::Paused));
        let mut resumed = Control::new(policy(3), ExecutionOperation::Process, true, 64).unwrap();
        assert!(resumed.enter(root).unwrap());
        assert_eq!(resumed.available(), 1);
        assert!(!resumed.reserve(root, 2).unwrap());
        assert_eq!(resumed.report.stop, Some(Stop::DayLimit));
        resumed.deadline = Some(Instant::now());
        assert!(!resumed.check_duration());
        assert_eq!(resumed.report.stop, Some(Stop::DayLimit));
        let mut other = Control::new(policy(3), ExecutionOperation::Knowledge, false, 64).unwrap();
        assert!(other.enter(root).unwrap());
        assert!(other.reserve(root, 1).unwrap());
        assert_eq!(other.available(), 0);
        assert_eq!(Usage::load(root, today()).unwrap().reserved_calls, 3);
    }

    #[test]
    fn only_a_later_utc_day_resets_calls_and_pauses_survive_rollover() {
        let directory = fixture();
        let root = directory.path();
        let date: Date = "2026-11-07".parse().unwrap();
        let mut usage = Usage::load(root, date).unwrap();
        usage.reserved_calls = 19;
        usage.paused.insert(
            ExecutionOperation::Daily,
            Pause {
                at: jiff::Timestamp::now(),
                cause: Some(HaltCause::new("daily ingest", None)),
            },
        );
        usage.save(root).unwrap();
        assert_eq!(Usage::load(root, date).unwrap().reserved_calls, 19);
        assert!(Usage::load(root, "2026-11-06".parse().unwrap()).is_err());
        let later = Usage::load(root, "2026-11-08".parse().unwrap()).unwrap();
        assert_eq!(later.reserved_calls, 0);
        assert!(later.paused.contains_key(&ExecutionOperation::Daily));
    }

    #[test]
    fn a_host_environment_pause_from_0_163_no_longer_holds_its_operation() {
        let directory = fixture();
        let root = directory.path();
        let mut usage = Usage::load(root, today()).unwrap();
        for (operation, stage) in [
            (ExecutionOperation::Knowledge, HOST_ENVIRONMENT_STAGE),
            (ExecutionOperation::Process, "native result host"),
        ] {
            usage.paused.insert(
                operation,
                Pause {
                    at: jiff::Timestamp::now(),
                    cause: Some(HaltCause::new(stage, Some("profile primary: missing"))),
                },
            );
        }
        usage.save(root).unwrap();
        let mut knowledge =
            Control::new(policy(3), ExecutionOperation::Knowledge, false, 64).unwrap();
        assert!(knowledge.preflight(root).unwrap());
        assert!(knowledge.enter(root).unwrap());
        let mut process = Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        assert!(!process.enter(root).unwrap());
        assert_eq!(process.report.stop, Some(Stop::Paused));
        let stored = Usage::load(root, today()).unwrap();
        assert_eq!(
            stored.paused.keys().copied().collect::<Vec<_>>(),
            [ExecutionOperation::Process]
        );
    }

    #[test]
    fn invalid_usage_never_becomes_a_fresh_allowance() {
        let directory = fixture();
        let root = directory.path();
        for bytes in [
            b"{}".as_slice(),
            br#"{"schema":"unknown","utc_date":"2026-11-07","reserved_calls":0,"paused":{}}"#,
            br#"{"schema":"automation-usage-v1","utc_date":"2026-11-07","reserved_calls":3,"reserved_calls":0,"paused":{}}"#,
            br#"{"schema":"automation-usage-v1","utc_date":"2026-11-07","reserved_calls":-1,"paused":{}}"#,
            br#"{"schema":"automation-usage-v2","utc_date":"2026-11-07","reserved_calls":0,"paused":{},"extra":0}"#,
            br#"{"schema":"automation-usage-v2","utc_date":"2026-11-07","reserved_calls":0,"paused":{"process":"2026-11-07T00:00:00Z"}}"#,
        ] {
            std::fs::write(path(root).unwrap(), bytes).unwrap();
            assert!(Usage::load(root, today()).is_err());
        }
        std::fs::remove_file(path(root).unwrap()).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), path(root).unwrap()).unwrap();
        assert!(Usage::load(root, today()).is_err());
    }

    #[test]
    fn pauses_keep_their_halting_stage_and_v1_pauses_load_without_one() {
        let directory = fixture();
        let root = directory.path();
        std::fs::write(
            path(root).unwrap(),
            format!(
                r#"{{"schema":"automation-usage-v1","utc_date":"{}","reserved_calls":2,"paused":{{"knowledge":"2026-11-09T00:00:00Z"}}}}"#,
                today()
            ),
        )
        .unwrap();
        let legacy = status(root, &policy(3)).unwrap();
        assert!(
            legacy.paused[&ExecutionOperation::Knowledge]
                .cause
                .is_none()
        );
        let mut control = Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        assert!(control.enter(root).unwrap());
        let detail = "é".repeat(MAX_HALT_DETAIL_BYTES);
        control
            .pause(root, HaltCause::new("native result host", Some(&detail)))
            .unwrap();
        let paused = status(root, &policy(3)).unwrap().paused;
        let cause = paused[&ExecutionOperation::Process].cause.as_ref().unwrap();
        assert_eq!(cause.stage, "native result host");
        let kept = cause.detail.as_deref().unwrap();
        assert!(kept.len() <= MAX_HALT_DETAIL_BYTES && kept.ends_with("é…"));
        assert!(paused[&ExecutionOperation::Knowledge].cause.is_none());
        assert!(
            std::fs::read_to_string(path(root).unwrap())
                .unwrap()
                .starts_with(r#"{"schema":"automation-usage-v2""#)
        );
    }

    #[test]
    fn an_elapsed_deadline_prevents_reservation_without_rewriting_usage() {
        let directory = fixture();
        let root = directory.path();
        let mut control = Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        control.enter(root).unwrap();
        control.reserve(root, 1).unwrap();
        let before = std::fs::read(path(root).unwrap()).unwrap();
        control.deadline = Some(Instant::now());
        assert!(!control.reserve(root, 1).unwrap());
        assert_eq!(control.report.stop, Some(Stop::RunDuration));
        assert_eq!(std::fs::read(path(root).unwrap()).unwrap(), before);
    }

    #[test]
    fn a_workflow_turn_yields_only_after_its_configured_hold() {
        let directory = fixture();
        let mut holding = Control::new(policy(8), ExecutionOperation::Knowledge, false, 4).unwrap();
        assert!(
            !holding.may_yield(),
            "a run without a turn has nothing to yield"
        );
        assert!(holding.enter(directory.path()).unwrap());
        assert!(!holding.may_yield());
        holding.turn_started = Some(Instant::now() - Duration::from_secs(600));
        assert!(holding.may_yield());
        assert!(holding.enter(directory.path()).unwrap());
        assert!(
            !holding.may_yield(),
            "a reacquired workflow starts a new turn"
        );
        let immediate = AutomationConfig {
            yield_after_seconds: 0,
            ..policy(8)
        };
        let mut immediate = Control::new(immediate, ExecutionOperation::Process, false, 4).unwrap();
        assert!(immediate.enter(directory.path()).unwrap());
        assert!(immediate.may_yield());
    }

    #[test]
    fn initial_workflow_wait_can_exceed_the_execution_budget_without_starting_its_clock() {
        let directory = fixture();
        let policy = AutomationConfig {
            max_run_seconds: 1.try_into().unwrap(),
            max_wait_seconds: Some(60.try_into().unwrap()),
            ..policy(8)
        };
        let mut control = Control::new(policy, ExecutionOperation::Knowledge, false, 4).unwrap();
        control.record_wait(Duration::from_secs(20)).unwrap();
        assert!(control.deadline.is_none());
        assert_eq!(control.report.workflow_wait_ms, 20_000);
        assert!(control.enter(directory.path()).unwrap());
        assert_eq!(control.report.stop, None);
        assert_eq!(control.available(), 4);
    }

    #[test]
    fn workflow_waiting_preserves_execution_time_and_accumulates_across_batches() {
        let directory = fixture();
        let mut control = Control::new(policy(8), ExecutionOperation::Process, false, 4).unwrap();
        control.record_wait(Duration::from_secs(20)).unwrap();
        assert!(control.deadline.is_none());
        assert!(control.enter(directory.path()).unwrap());
        assert!(control.reserve(directory.path(), 1).unwrap());
        let deadline = control.deadline.unwrap();
        control.record_wait(Duration::from_secs(10)).unwrap();
        assert_eq!(control.deadline, Some(deadline + Duration::from_secs(10)));
        assert!(control.enter(directory.path()).unwrap());
        assert_eq!(control.deadline, Some(deadline + Duration::from_secs(10)));
        assert_eq!(control.report.run_reservations, 1);
        assert_eq!(control.report.workflow_wait_ms, 30_000);
        assert_eq!(control.wait_remaining, Duration::from_secs(30));
        assert!(control.wait_deadline().unwrap().is_some());
    }

    #[test]
    fn exhausted_waiting_does_not_reserve_calls_or_change_the_usage_ledger() {
        let directory = fixture();
        let mut control = Control::new(policy(8), ExecutionOperation::Knowledge, false, 4).unwrap();
        assert!(control.enter(directory.path()).unwrap());
        assert!(control.reserve(directory.path(), 1).unwrap());
        let before = std::fs::read(path(directory.path()).unwrap()).unwrap();
        control.record_wait(Duration::from_secs(60)).unwrap();
        assert!(control.wait_deadline().unwrap().is_none());
        assert_eq!(control.report.stop, Some(Stop::WaitDuration));
        assert!(!control.reserve(directory.path(), 1).unwrap());
        assert_eq!(control.report.run_reservations, 1);
        assert_eq!(
            std::fs::read(path(directory.path()).unwrap()).unwrap(),
            before
        );
    }

    #[test]
    fn batch_boundaries_do_not_reset_the_scheduled_invocation_allowance() {
        let directory = fixture();
        let root = directory.path();
        let mut control =
            Control::new(policy(256), ExecutionOperation::Knowledge, false, 2).unwrap();
        assert!(control.enter(root).unwrap());
        assert!(control.reserve(root, 1).unwrap());
        assert!(control.enter(root).unwrap());
        assert!(control.reserve(root, 1).unwrap());
        assert!(control.enter(root).unwrap());
        assert!(!control.has_remaining_budget());
        assert_eq!(control.report.stop, None);
        assert!(!control.reserve(root, 1).unwrap());
        assert_eq!(control.report.stop, Some(Stop::RunLimit));
        assert_eq!(Usage::load(root, today()).unwrap().reserved_calls, 2);
    }

    #[test]
    fn unlimited_daily_policy_preserves_run_limits_usage_and_pauses_across_policy_changes() {
        let directory = fixture();
        let root = directory.path();
        let mut capped = Control::new(policy(2), ExecutionOperation::Knowledge, false, 2).unwrap();
        assert!(capped.enter(root).unwrap());
        assert!(capped.reserve(root, 2).unwrap());
        let mut unlimited = policy(2);
        unlimited.max_calls_per_day = None;
        let mut control =
            Control::new(unlimited.clone(), ExecutionOperation::Knowledge, false, 2).unwrap();
        assert!(control.enter(root).unwrap());
        assert_eq!(control.available(), 2);
        assert!(control.reserve(root, 2).unwrap());
        assert_eq!(control.available(), 0);
        control.stop_for_limit();
        assert_eq!(control.report.stop, Some(Stop::RunLimit));
        let snapshot = status(root, &unlimited).unwrap();
        assert_eq!(snapshot.reserved_calls, 4);
        assert!(snapshot.max_calls_per_day.is_none());
        assert!(snapshot.remaining_calls.is_none());
        control
            .pause(root, HaltCause::new("native result host", None))
            .unwrap();
        let before = std::fs::read(path(root).unwrap()).unwrap();
        let mut restarted =
            Control::new(unlimited, ExecutionOperation::Knowledge, false, 2).unwrap();
        assert!(!restarted.enter(root).unwrap());
        assert_eq!(restarted.report.stop, Some(Stop::Paused));
        assert_eq!(std::fs::read(path(root).unwrap()).unwrap(), before);
        let mut recapped = Control::new(policy(3), ExecutionOperation::Knowledge, true, 2).unwrap();
        assert!(recapped.enter(root).unwrap());
        assert_eq!(recapped.available(), 0);
        assert!(!recapped.reserve(root, 1).unwrap());
        assert_eq!(recapped.report.stop, Some(Stop::DayLimit));
        let usage = Usage::load(root, today()).unwrap();
        assert_eq!(usage.reserved_calls, 4);
        assert!(usage.paused.is_empty());
    }

    #[test]
    fn unlimited_daily_policy_refuses_counter_overflow_without_replacing_usage() {
        let directory = fixture();
        let root = directory.path();
        let mut usage = Usage::load(root, today()).unwrap();
        usage.reserved_calls = usize::MAX;
        usage.save(root).unwrap();
        let before = std::fs::read(path(root).unwrap()).unwrap();
        let mut unlimited = policy(2);
        unlimited.max_calls_per_day = None;
        let mut control = Control::new(unlimited, ExecutionOperation::Process, false, 2).unwrap();
        assert!(control.enter(root).unwrap());
        assert_eq!(control.available(), 2);
        assert!(control.reserve(root, 1).is_err());
        assert_eq!(std::fs::read(path(root).unwrap()).unwrap(), before);
    }
}
