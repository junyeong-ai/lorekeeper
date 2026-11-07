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

#[derive(Clone, Copy, Deserialize, Serialize)]
enum Schema {
    #[serde(rename = "automation-usage-v1")]
    V1,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    schema: Schema,
    utc_date: Date,
    reserved_calls: usize,
    paused: BTreeMap<ExecutionOperation, jiff::Timestamp>,
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
                    schema: Schema::V1,
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
        let value = crate::commands::json::decode_object(&bytes).map_err(|_| {
            miette::miette!("automation usage requires a JSON object with unique fields")
        })?;
        let mut usage: Self = serde_json::from_value(value)
            .map_err(|_| miette::miette!("invalid automation usage schema or fields"))?;
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
    Paused,
}

#[derive(Clone, Serialize)]
pub(super) struct Report {
    pub utc_date: Option<Date>,
    pub reserved_calls: Option<usize>,
    pub max_calls_per_day: usize,
    pub max_calls_per_run: usize,
    pub run_reservations: usize,
    pub stop: Option<Stop>,
}

/// The native Runner serializes this operational ledger under its acquired
/// physical-vault workflow lease. Synchronous atomic reservations precede host
/// launch; they are independent of canonical mutation journals and are never refunded.
pub(super) struct Control {
    policy: AutomationConfig,
    operation: ExecutionOperation,
    resume: bool,
    deadline: Instant,
    pub report: Report,
}

fn today() -> Date {
    jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
}

#[derive(Serialize)]
pub(in crate::commands) struct Status {
    pub utc_date: Date,
    pub reserved_calls: usize,
    pub max_calls_per_day: usize,
    pub remaining_calls: usize,
    pub max_run_seconds: u64,
    pub paused: BTreeMap<ExecutionOperation, jiff::Timestamp>,
}

pub(in crate::commands) fn status(
    root: &Path,
    policy: &AutomationConfig,
) -> miette::Result<Status> {
    let usage = Usage::load(root, today())?;
    Ok(Status {
        utc_date: usage.utc_date,
        reserved_calls: usage.reserved_calls,
        max_calls_per_day: policy.max_calls_per_day.get(),
        remaining_calls: policy
            .max_calls_per_day
            .get()
            .saturating_sub(usage.reserved_calls),
        max_run_seconds: policy.max_run_seconds.get(),
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
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(policy.max_run_seconds.get()))
            .ok_or_else(|| miette::miette!("automation duration exceeds the monotonic clock"))?;
        Ok(Self {
            report: Report {
                utc_date: None,
                reserved_calls: None,
                max_calls_per_day: policy.max_calls_per_day.get(),
                max_calls_per_run,
                run_reservations: 0,
                stop: None,
            },
            policy,
            operation,
            resume,
            deadline,
        })
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn available(&self) -> usize {
        self.policy
            .max_calls_per_day
            .get()
            .saturating_sub(
                self.report
                    .reserved_calls
                    .expect("usage read under owned workflow"),
            )
            .min(
                self.report
                    .max_calls_per_run
                    .saturating_sub(self.report.run_reservations),
            )
    }

    pub fn check_duration(&mut self) -> bool {
        if self.report.stop.is_none() && Instant::now() >= self.deadline {
            self.report.stop = Some(Stop::RunDuration);
        }
        self.report.stop.is_none()
    }

    pub fn enter(&mut self, root: &Path) -> miette::Result<bool> {
        if !self.check_duration() {
            return Ok(false);
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
        if reserved > self.policy.max_calls_per_day.get() {
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
            if self.report.reserved_calls.expect("admitted usage")
                >= self.policy.max_calls_per_day.get()
            {
                Stop::DayLimit
            } else {
                Stop::RunLimit
            },
        );
    }

    pub fn pause(&mut self, root: &Path) -> miette::Result<()> {
        let mut usage = Usage::load(root, today())?;
        usage.paused.insert(self.operation, jiff::Timestamp::now());
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
            max_calls_per_day: limit.try_into().unwrap(),
            max_run_seconds: 60.try_into().unwrap(),
        }
    }

    #[test]
    fn reservations_survive_restart_and_resume_does_not_reset_usage() {
        let directory = fixture();
        let root = directory.path();
        let mut first = Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        assert!(first.enter(root).unwrap());
        assert!(first.reserve(root, 2).unwrap());
        first.pause(root).unwrap();
        let mut restarted =
            Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        assert!(!restarted.enter(root).unwrap());
        assert_eq!(restarted.report.stop, Some(Stop::Paused));
        let mut resumed = Control::new(policy(3), ExecutionOperation::Process, true, 64).unwrap();
        assert!(resumed.enter(root).unwrap());
        assert_eq!(resumed.available(), 1);
        assert!(!resumed.reserve(root, 2).unwrap());
        assert_eq!(resumed.report.stop, Some(Stop::DayLimit));
        resumed.deadline = Instant::now();
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
        usage
            .paused
            .insert(ExecutionOperation::Daily, jiff::Timestamp::now());
        usage.save(root).unwrap();
        assert_eq!(Usage::load(root, date).unwrap().reserved_calls, 19);
        assert!(Usage::load(root, "2026-11-06".parse().unwrap()).is_err());
        let later = Usage::load(root, "2026-11-08".parse().unwrap()).unwrap();
        assert_eq!(later.reserved_calls, 0);
        assert!(later.paused.contains_key(&ExecutionOperation::Daily));
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
        ] {
            std::fs::write(path(root).unwrap(), bytes).unwrap();
            assert!(Usage::load(root, today()).is_err());
        }
        std::fs::remove_file(path(root).unwrap()).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), path(root).unwrap()).unwrap();
        assert!(Usage::load(root, today()).is_err());
    }

    #[test]
    fn an_elapsed_deadline_prevents_reservation_without_rewriting_usage() {
        let directory = fixture();
        let root = directory.path();
        let mut control = Control::new(policy(3), ExecutionOperation::Process, false, 64).unwrap();
        control.enter(root).unwrap();
        control.reserve(root, 1).unwrap();
        let before = std::fs::read(path(root).unwrap()).unwrap();
        control.deadline = Instant::now();
        assert!(!control.reserve(root, 1).unwrap());
        assert_eq!(control.report.stop, Some(Stop::RunDuration));
        assert_eq!(std::fs::read(path(root).unwrap()).unwrap(), before);
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
        assert!(!control.reserve(root, 1).unwrap());
        assert_eq!(control.report.stop, Some(Stop::RunLimit));
        assert_eq!(Usage::load(root, today()).unwrap().reserved_calls, 2);
    }
}
