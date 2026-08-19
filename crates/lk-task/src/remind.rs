//! A promise to interrupt someone at a time.
//!
//! Kept OUTSIDE the board, which is the one decision here worth arguing. A reminder is
//! forward-looking and a person's own, so the board is where it looks like it belongs — but a
//! reminder is fired by a TIMER, and a timer that rewrites the board every few minutes writes
//! the person's own file underneath an open editor and a sync client, on a schedule, forever.
//! The kernel lock cannot reach the other machine. So reminders live in their own store, where
//! firing one costs a write nobody else is holding.
//!
//! What stays on the board is `wake:` — a promise to RESURFACE, which is a state change and
//! belongs to the state machine. These two are not the same thing at two resolutions: one
//! changes what the day is committed to, the other only says something out loud.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::TaskError;
use crate::task::TaskId;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reminder {
    /// Minted the same way a task's id is, and from the same alphabet, so a person reading one
    /// out of a notification can name it back.
    pub id: TaskId,
    pub at: jiff::Timestamp,
    pub text: String,
    /// The task this is about, where it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
}

/// Everything promised and not yet said.
///
/// One file rather than one per date: the question is always "what is due now", never "what did
/// the 14th hold", and a fired reminder leaves no history worth keeping — what it was ABOUT is
/// the task, which has its own.
pub struct Reminders {
    file: crate::store::Jsonl,
}

impl Reminders {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            file: crate::store::Jsonl::at(
                vault_root
                    .join(".lorekeeper")
                    .join("reminders")
                    .join("pending.jsonl"),
            ),
        }
    }

    pub fn read(&self) -> Result<Vec<Reminder>, TaskError> {
        let mut held: Vec<Reminder> = self.file.read()?;
        held.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
        Ok(held)
    }

    pub fn add(&self, reminder: Reminder) -> Result<(), TaskError> {
        let mut held = self.read()?;
        held.push(reminder);
        self.file.replace(&held)
    }

    pub fn remove(&self, id: &TaskId) -> Result<bool, TaskError> {
        let mut held = self.read()?;
        let before = held.len();
        held.retain(|reminder| &reminder.id != id);
        if held.len() == before {
            return Ok(false);
        }
        self.file.replace(&held)?;
        Ok(true)
    }

    /// Everything due at or before `now`. A READ — nothing is removed here.
    ///
    /// Retiring is [`Self::answered`], which the caller reaches only once it has decided what
    /// each one is. Doing both in this call put the write BEFORE the decision: a caller that
    /// then dropped a reminder — because a board it could not parse could not say the task was
    /// still open — had already written it out of the store, so a promise this guarantees is
    /// late became one that was gone.
    pub fn due(&self, now: jiff::Timestamp) -> Result<Vec<Reminder>, TaskError> {
        Ok(self
            .read()?
            .into_iter()
            .filter(|reminder| reminder.at <= now)
            .collect())
    }

    /// Retire the reminders that have been ANSWERED — said out loud, or established as moot.
    ///
    /// The same rule a wake date follows: it was a promise to say something once, and one that
    /// survived would be said again every few minutes for the rest of the day. What is NOT
    /// answered stays, so a reminder due while the machine slept, or while the board could not
    /// say whether its task is still open, is late rather than lost.
    pub fn answered(&self, ids: &[TaskId]) -> Result<(), TaskError> {
        if ids.is_empty() {
            return Ok(());
        }
        let keep: Vec<_> = self
            .read()?
            .into_iter()
            .filter(|reminder| !ids.contains(&reminder.id))
            .collect();
        self.file.replace(&keep)
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

    fn reminder(id: &str, hour: i8) -> Reminder {
        Reminder {
            id: id.parse().unwrap(),
            at: at(hour),
            text: format!("say something at {hour}"),
            task: None,
        }
    }

    #[test]
    fn what_is_due_is_read_and_retired_only_once_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let reminders = Reminders::new(tmp.path());
        reminders.add(reminder("7k2p", 9)).unwrap();
        reminders.add(reminder("3b8q", 14)).unwrap();

        let fired = reminders.due(at(10)).unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id.as_str(), "7k2p");

        // Reading decides nothing. A caller that could not answer leaves it waiting.
        assert_eq!(reminders.due(at(10)).unwrap().len(), 1);
        assert_eq!(reminders.read().unwrap().len(), 2);

        reminders.answered(&["7k2p".parse().unwrap()]).unwrap();
        assert!(reminders.due(at(10)).unwrap().is_empty());
        assert_eq!(reminders.read().unwrap().len(), 1);
    }

    /// The guarantee this store exists to keep: nothing but being ANSWERED removes one.
    #[test]
    fn one_nobody_could_answer_is_late_rather_than_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let reminders = Reminders::new(tmp.path());
        reminders.add(reminder("7k2p", 9)).unwrap();

        // Three ticks where the caller could decide nothing.
        for _ in 0..3 {
            assert_eq!(reminders.due(at(17)).unwrap().len(), 1);
        }
        assert_eq!(reminders.read().unwrap().len(), 1);
    }

    /// A machine asleep at 14:00 wakes at 17:00 and the reminder is still there.
    #[test]
    fn one_due_while_the_machine_slept_is_still_due() {
        let tmp = tempfile::tempdir().unwrap();
        let reminders = Reminders::new(tmp.path());
        reminders.add(reminder("7k2p", 14)).unwrap();
        assert_eq!(reminders.due(at(17)).unwrap().len(), 1);
    }

    #[test]
    fn one_can_be_taken_back() {
        let tmp = tempfile::tempdir().unwrap();
        let reminders = Reminders::new(tmp.path());
        reminders.add(reminder("7k2p", 9)).unwrap();
        assert!(reminders.remove(&"7k2p".parse().unwrap()).unwrap());
        assert!(!reminders.remove(&"7k2p".parse().unwrap()).unwrap());
        assert!(reminders.read().unwrap().is_empty());
    }

    #[test]
    fn a_vault_with_no_reminders_has_none_due() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Reminders::new(tmp.path()).due(at(9)).unwrap().is_empty());
    }
}
