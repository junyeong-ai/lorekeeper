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

/// One source's appointments, replaced whole by each ingest that reaches it.
///
/// A SNAPSHOT per source rather than a file per date, which is the shape of the thing: a
/// calendar re-fetches its whole window on demand, so what it did not observe this time is not
/// on the calendar. Keyed by date it could not say that — a date with no events produced no
/// entry, so `record` was never called for it and a day whose every meeting was cancelled kept
/// showing them until the retention horizon. Keyed by source, cancelling them all is an empty
/// snapshot, which is an answer.
///
/// It also stops being something to prune. A snapshot holds one window rather than one file per
/// day forever, and a date outside that window has no schedule here — the calendar's own daily
/// page is the durable record of a day that has passed.
pub struct Schedule {
    shelf: crate::store::Shelf,
}

impl Schedule {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            shelf: crate::store::Shelf::at(vault_root.join(".lorekeeper").join("agenda")),
        }
    }

    pub fn record(&self, source_id: &str, appointments: &[Appointment]) -> Result<(), TaskError> {
        let mut held = appointments.to_vec();
        held.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.title.cmp(&b.title)));
        self.shelf.file(source_id).replace(&held)
    }

    /// What `date` holds, earliest first, across every source.
    ///
    /// A source whose snapshot will not read is REPORTED by the error rather than skipped: the
    /// agenda would otherwise show a day with a meeting missing from it, which reads exactly
    /// like a day that has none.
    pub fn read(
        &self,
        date: jiff::civil::Date,
        zone: &jiff::tz::TimeZone,
    ) -> Result<Vec<Appointment>, TaskError> {
        let mut held = Vec::new();
        for key in self.shelf.keys()? {
            held.extend(
                self.shelf
                    .file(&key)
                    .read::<Appointment>()?
                    .into_iter()
                    .filter(|a| a.at.to_zoned(zone.clone()).date() == date),
            );
        }
        held.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.title.cmp(&b.title)));
        Ok(held)
    }

    /// The snapshots no configured source answers for.
    pub fn orphans(&self, configured: &[String]) -> Result<Vec<PathBuf>, TaskError> {
        Ok(self
            .shelf
            .keys()?
            .into_iter()
            .filter(|key| !configured.contains(key))
            .map(|key| self.shelf.file(&key).path().to_path_buf())
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
    fn a_source_is_replaced_whole_and_read_by_date() {
        let tmp = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(tmp.path());
        let utc = jiff::tz::TimeZone::UTC;
        let day = jiff::civil::date(2026, 8, 19);

        schedule
            .record(
                "calendar",
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
        assert_eq!(
            schedule
                .read(day, &utc)
                .unwrap()
                .iter()
                .map(|a| a.title.as_str())
                .collect::<Vec<_>>(),
            ["standup", "retro"]
        );

        // Every meeting on the day was cancelled. Keyed by date this could not be said, so the
        // day went on showing them.
        schedule.record("calendar", &[]).unwrap();
        assert!(schedule.read(day, &utc).unwrap().is_empty());
    }

    #[test]
    fn two_calendars_are_one_day() {
        let tmp = tempfile::tempdir().unwrap();
        let schedule = Schedule::new(tmp.path());
        let utc = jiff::tz::TimeZone::UTC;
        schedule
            .record(
                "work",
                &[Appointment {
                    at: at(9),
                    title: "standup".into(),
                }],
            )
            .unwrap();
        schedule
            .record(
                "personal",
                &[Appointment {
                    at: at(7),
                    title: "gym".into(),
                }],
            )
            .unwrap();
        assert_eq!(
            schedule
                .read(jiff::civil::date(2026, 8, 19), &utc)
                .unwrap()
                .iter()
                .map(|a| a.title.as_str())
                .collect::<Vec<_>>(),
            ["gym", "standup"]
        );
    }

    #[test]
    fn a_date_never_ingested_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            Schedule::new(tmp.path())
                .read(jiff::civil::date(2026, 8, 19), &jiff::tz::TimeZone::UTC)
                .unwrap()
                .is_empty()
        );
    }
}
