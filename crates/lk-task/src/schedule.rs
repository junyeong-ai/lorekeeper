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
    shelf: crate::store::Shelf,
}

impl Schedule {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            shelf: crate::store::Shelf::at(vault_root.join(".lorekeeper").join("agenda")),
        }
    }

    pub fn record(
        &self,
        date: jiff::civil::Date,
        appointments: &[Appointment],
    ) -> Result<(), TaskError> {
        let mut held = appointments.to_vec();
        held.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.title.cmp(&b.title)));
        self.shelf.file(&date.to_string()).replace(&held)
    }

    /// What `date` holds, earliest first.
    ///
    /// A date that was never ingested has none, which is not an error: the agenda is a view and
    /// answers with what it can see.
    pub fn read(&self, date: jiff::civil::Date) -> Result<Vec<Appointment>, TaskError> {
        self.shelf.file(&date.to_string()).read()
    }

    /// The dates whose appointments are older than `cutoff`.
    ///
    /// A rendering aid rather than a record: the vault's own daily page holds what a calendar
    /// observed, so a schedule past the retention horizon is operational history and is pruned
    /// exactly as the ingest log is. Without this the directory gains a file a day, forever,
    /// for a view that only ever asks about one of them.
    pub fn expired(&self, cutoff: jiff::civil::Date) -> Result<Vec<PathBuf>, TaskError> {
        Ok(self
            .shelf
            .dates()?
            .into_iter()
            .filter(|date| *date < cutoff)
            .map(|date| self.shelf.file(&date.to_string()).path().to_path_buf())
            .collect())
    }

    pub fn retire(paths: &[PathBuf]) -> Result<(), TaskError> {
        for path in paths {
            crate::store::Jsonl::at(path.clone()).retire()?;
        }
        Ok(())
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
