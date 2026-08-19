//! The intent plane: what the user means to do, and how it becomes what they did.
//!
//! Everything else in this workspace runs in one temporal direction — an external source is
//! observed, the observation becomes a page, the pages become knowledge — and every invariant
//! enforces it: the vault is realized-only, forecasts never materialize, the work-log records
//! performed contribution. A task is the other direction. It is a claim about the future,
//! authored by a person, that changes state by their hand rather than by a re-fetch.
//!
//! So it is a separate plane rather than a new kind of page, and it touches the existing two at
//! exactly two points. An observation may PROPOSE a task and never creates one. A completed task
//! BECOMES an observation — [`Transition::observation`] hands the ingest pipeline an ordinary
//! `RawItem`, and from there the daily page, the work-log, the contribution categories, the
//! concept extraction and every review consume it without a line of them changing. That second
//! joint is the whole archive: there is no archival machinery here, because finishing something
//! is an event, and this workspace already knows what to do with an event.
//!
//! The board is a markdown file and it is the TRUTH, not a rendering of a store kept elsewhere.
//! The vault is the product: a box ticked on a phone has to count, and a design that keeps state
//! somewhere else discards that edit in silence. What keeps the parsing risk small is that a
//! line this cannot read is never rewritten.

mod board;
mod log;
mod propose;
mod reconcile;
mod remind;
mod schedule;
mod store;
mod task;

pub use board::{Board, Entry};
pub use log::{Recorded, Transition, TransitionKind, TransitionLog};
pub use propose::{Candidate, Candidates, Gathered, Judged, select};
pub use reconcile::{Reconciled, rollover, sync};
pub use remind::{Reminder, Reminders};
pub use schedule::{Appointment, Schedule};
pub use task::{Task, TaskId, TaskState};

use thiserror::Error;

/// The `id` and `type` the board page declares.
///
/// A page states its own format so a directory can be recognized as holding this tool's output
/// without guessing at content — the same key every other page carries, and the one thing OKF
/// requires.
pub const BOARD_ID: &str = "tasks";
pub use lk_core::vault_path::TASK_BOARD_FORMAT as BOARD_FORMAT;

#[derive(Debug, Error)]
pub enum TaskError {
    /// A line, a stamp or an argument this refuses to read. Never repaired by guessing: the
    /// bytes belong to whoever wrote them, and a guess at what a broken date meant is a task
    /// silently rescheduled.
    #[error("{0}")]
    Malformed(String),
    #[error("no task answers to `{0}`")]
    Absent(TaskId),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl TaskError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
