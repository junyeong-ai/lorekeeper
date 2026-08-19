//! The day's appointments, as the agenda reads them.
//!
//! Observation data rather than intent, kept here because `lore agenda` is its only consumer
//! and the view's two inputs are better read from one place than assembled from two crates.
//!
//! An appointment is NOT proposed. A meeting is a time already committed to, not work to decide
//! about, and putting one on the board would give a person a line to clear every morning for
//! something that happens whether they clear it or not. So the agenda reports it beside the
//! day's tasks and the board never learns of it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::TaskError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Appointment {
    pub at: jiff::Timestamp,
    pub title: String,
}

/// One date's appointments, replaced whole by each ingest that reaches the source.
///
/// A calendar re-fetches its window on demand, so what a run did not observe for a date is no
/// longer on it — a cancelled meeting stops appearing with nothing to reconcile.
pub struct Schedule {
    root: PathBuf,
}

impl Schedule {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            root: vault_root.join(".lorekeeper").join("agenda"),
        }
    }

    fn path(&self, date: jiff::civil::Date) -> PathBuf {
        self.root.join(format!("{date}.jsonl"))
    }

    pub fn record(
        &self,
        date: jiff::civil::Date,
        appointments: &[Appointment],
    ) -> Result<(), TaskError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| TaskError::io(format!("create {}", self.root.display()), e))?;
        let mut held = appointments.to_vec();
        held.sort_by_key(|a| (a.at, a.title.clone()));
        let mut buf = String::new();
        for appointment in &held {
            let line = serde_json::to_string(appointment)
                .map_err(|e| TaskError::Malformed(format!("serialize appointment: {e}")))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        let path = self.path(date);
        lk_core::fs::write_atomic(&path, buf.as_bytes(), None)
            .map_err(|e| TaskError::io(format!("write {}", path.display()), e))
    }

    /// What `date` holds, earliest first.
    ///
    /// A date that was never ingested has none, which is not an error: the agenda is a view and
    /// answers with what it can see.
    pub fn read(&self, date: jiff::civil::Date) -> Result<Vec<Appointment>, TaskError> {
        let path = self.path(date);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(TaskError::io(format!("read {}", path.display()), e)),
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|e| {
                    TaskError::Malformed(format!("{} is corrupt: {e}", path.display()))
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: i8) -> jiff::Timestamp {
        jiff::civil::date(2026, 8, 19)
            .at(hour, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    #[test]
    fn a_date_is_replaced_whole_and_read_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(tmp.path());
        let day = jiff::civil::date(2026, 8, 19);

        schedule
            .record(
                day,
                &[
                    Appointment {
                        at: at(15),
                        title: "retro".into(),
                    },
                    Appointment {
                        at: at(9),
                        title: "standup".into(),
                    },
                ],
            )
            .unwrap();
        let held = schedule.read(day).unwrap();
        assert_eq!(
            held.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(),
            ["standup", "retro"]
        );

        // The retro was cancelled, so the next ingest simply does not observe it.
        schedule
            .record(
                day,
                &[Appointment {
                    at: at(9),
                    title: "standup".into(),
                }],
            )
            .unwrap();
        assert_eq!(schedule.read(day).unwrap().len(), 1);
    }

    #[test]
    fn a_date_never_ingested_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(tmp.path());
        assert!(
            schedule
                .read(jiff::civil::date(2026, 8, 19))
                .unwrap()
                .is_empty()
        );
    }
}
