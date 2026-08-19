use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::TaskError;

/// The address a task answers to for as long as it exists.
///
/// A task's TEXT is expected to change — refining what a thing actually is, is most of what
/// keeping a list is — so the text cannot be the identity, for the same reason a concept's slug
/// is resolved once and never re-derived from its title. Four characters of a base32 alphabet
/// with no ambiguous glyphs: short enough to type without copying, and checked against the
/// board it is minted into rather than assumed unique.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(String);

/// Crockford's alphabet minus nothing: it already omits `i`, `l`, `o` and `u`, which is exactly
/// what keeps a hand-typed id from resolving to another task.
const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";
const ID_LEN: usize = 4;

impl TaskId {
    /// The id `seed` mints, skipping any already taken.
    ///
    /// Derived rather than drawn: the caller supplies the seed (a title and the instant it was
    /// added), so a test mints a known id and two runs over the same board produce the same
    /// answer. Collisions are resolved by re-deriving from the previous id rather than by
    /// counting, so the result never depends on how many tasks happened to be scanned first.
    pub fn mint(seed: &str, taken: &BTreeSet<TaskId>) -> TaskId {
        let mut digest = blake3::hash(seed.as_bytes());
        loop {
            let candidate = TaskId(
                digest.as_bytes()[..ID_LEN]
                    .iter()
                    .map(|byte| ALPHABET[usize::from(*byte) % ALPHABET.len()] as char)
                    .collect(),
            );
            if !taken.contains(&candidate) {
                return candidate;
            }
            digest = blake3::hash(digest.as_bytes());
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for TaskId {
    type Err = TaskError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s.trim();
        if id.len() == ID_LEN && id.bytes().all(|b| ALPHABET.contains(&b)) {
            return Ok(TaskId(id.to_string()));
        }
        Err(TaskError::Malformed(format!(
            "`{s}` is not a task id ({ID_LEN} characters of {})",
            String::from_utf8_lossy(ALPHABET)
        )))
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a task sits on the board.
///
/// The board's section headings ARE these states, so moving a line between two headings in an
/// editor is a state change — which is what keeps the whole thing usable by someone who never
/// runs the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    /// Committed to today.
    Today,
    /// Work an OBSERVATION proposed and the person has not answered yet.
    ///
    /// The one state nothing else here reaches: a source put it there, so until someone moves
    /// it out it is not a commitment. Accepting is dragging the line into another section,
    /// which the state machine already reads; declining is `lore task drop`, which the history
    /// already records. That is why a proposal is an ordinary task line rather than a fifth
    /// kind of thing — the two answers a person can give already exist.
    Proposed,
    /// Ready, not committed.
    Next,
    /// Blocked on something that is not this list.
    Waiting,
    /// Kept, not scheduled.
    Someday,
}

impl TaskState {
    /// Board order, which is also agenda order: what is committed first, what is parked last.
    pub const ALL: [TaskState; 5] = [
        TaskState::Today,
        TaskState::Proposed,
        TaskState::Next,
        TaskState::Waiting,
        TaskState::Someday,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Today => "today",
            TaskState::Proposed => "proposed",
            TaskState::Next => "next",
            TaskState::Waiting => "waiting",
            TaskState::Someday => "someday",
        }
    }

    /// The heading this state is written under, in the vault's locale.
    pub fn heading(self, locale: lk_core::i18n::Locale) -> &'static str {
        let strings = locale.strings();
        match self {
            TaskState::Today => strings.tasks_today,
            TaskState::Proposed => strings.tasks_proposed,
            TaskState::Next => strings.tasks_next,
            TaskState::Waiting => strings.tasks_waiting,
            TaskState::Someday => strings.tasks_someday,
        }
    }
}

impl FromStr for TaskState {
    type Err = TaskError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TaskState::ALL
            .into_iter()
            .find(|state| state.as_str() == s.trim().to_lowercase())
            .ok_or_else(|| {
                TaskError::Malformed(format!(
                    "`{s}` is not a task state (today, next, waiting, someday)"
                ))
            })
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One open task.
///
/// Everything here except `title` is written into an HTML comment the line carries, so the
/// visible half is what a person reads and edits and the machine half is invisible in both
/// Obsidian's preview and GitHub's — the same rule the vault's links follow, applied to a
/// second kind of markup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub state: TaskState,
    /// The day this task was first written down. Not when it was committed to today, which is
    /// what `carried` counts from.
    pub since: jiff::civil::Date,
    pub due: Option<jiff::civil::Date>,
    /// The day a waiting task comes back. A date that has arrived is what `sync` acts on; one
    /// that has not is inert, so nothing about a task is ever materialized ahead of its day.
    pub wake: Option<jiff::civil::Date>,
    /// How many day-closes this task has survived under `Today`.
    ///
    /// Counted rather than derived from `since`: a task written down last month and committed
    /// to today yesterday is on its second day, not its thirtieth. A task carried five times is
    /// not a nag — it is a diagnosis, and it is what the weekly view is for.
    pub carried: u32,
    /// The day the last carry closed.
    ///
    /// Recorded because "has this already been carried" cannot be answered from the day the
    /// command RAN: closing the day by hand at 23:00 and again from the scheduled pipeline at
    /// 07:00 are two closes of one ended day, and their two records live in two different date
    /// files, so each read the other's as empty and the count went up twice for one day.
    pub carried_on: Option<jiff::civil::Date>,
    /// `true` where the line was checked off in an editor rather than closed through a command.
    /// Carried through parsing so the reconciler can harvest it; a rendered board never holds
    /// one, because harvesting removes the line.
    pub done: bool,
    /// What OBSERVATION this task answers to, as `blake3(url)[..16]`.
    ///
    /// The identity of the origin, as opposed to how it reads — the same split `t:` makes for
    /// the task itself, and for the same reason: the visible link is part of a title a person
    /// is expected to rewrite, and a join that read it would stop matching the moment they did.
    /// It is what stops one Jira issue being proposed every morning after it was accepted,
    /// declined or finished, and it is a hash rather than the URL because a stamp value is
    /// `[0-9A-Za-z-]+` and a URL is not.
    pub src: Option<String>,
    /// Stamp fields this build does not know, kept in the order they were written.
    ///
    /// A board is one file two builds may open — a laptop updated this morning and a desktop
    /// that has not been, on a vault they both sync. Refusing a field the writer added would
    /// take the whole line out of every rule on the older machine: not listed, not carried, not
    /// harvested, and looking perfectly normal in Obsidian the entire time. So an unknown field
    /// is carried through untouched, which is the same rule the headings already follow — read
    /// everything, write what this build knows.
    pub extra: Vec<(String, String)>,
}

impl Task {
    pub fn new(
        id: TaskId,
        title: impl Into<String>,
        state: TaskState,
        since: jiff::civil::Date,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            state,
            since,
            due: None,
            wake: None,
            carried: 0,
            carried_on: None,
            done: false,
            src: None,
            extra: Vec::new(),
        }
    }

    /// Whether this task asks for attention on `date`: committed to it, due by it, or a
    /// waiting task whose day has come.
    pub fn is_active_on(&self, date: jiff::civil::Date) -> bool {
        match self.state {
            TaskState::Today => true,
            TaskState::Waiting => self.wake.is_some_and(|wake| wake <= date),
            // A proposal asks to be ANSWERED, not worked on, so it never joins the day's
            // commitments — the agenda reports it separately. Reading it as active would put
            // work nobody accepted into the same list as work somebody did.
            TaskState::Proposed => false,
            TaskState::Next | TaskState::Someday => self.due.is_some_and(|due| due <= date),
        }
    }

    pub fn is_overdue_on(&self, date: jiff::civil::Date) -> bool {
        self.due.is_some_and(|due| due < date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i16, m: i8, d: i8) -> jiff::civil::Date {
        jiff::civil::date(y, m, d)
    }

    #[test]
    fn an_id_is_four_unambiguous_characters() {
        let id = TaskId::mint("a task", &BTreeSet::new());
        assert_eq!(id.as_str().len(), ID_LEN);
        assert!(id.as_str().bytes().all(|b| ALPHABET.contains(&b)));
        for confusable in ['i', 'l', 'o', 'u'] {
            assert!(!id.as_str().contains(confusable), "{id}");
        }
    }

    #[test]
    fn the_same_seed_mints_the_same_id() {
        let taken = BTreeSet::new();
        assert_eq!(TaskId::mint("seed", &taken), TaskId::mint("seed", &taken));
    }

    /// Two tasks may be added in the same second with the same text; the second one takes the
    /// next id its own seed derives rather than one the scan order decided.
    #[test]
    fn a_taken_id_is_stepped_past_deterministically() {
        let first = TaskId::mint("seed", &BTreeSet::new());
        let taken: BTreeSet<TaskId> = [first.clone()].into_iter().collect();
        let second = TaskId::mint("seed", &taken);
        assert_ne!(first, second);
        assert_eq!(
            second,
            TaskId::mint("seed", &taken),
            "and it is reproducible"
        );
    }

    #[test]
    fn an_id_round_trips_through_its_text() {
        let id = TaskId::mint("x", &BTreeSet::new());
        assert_eq!(id.as_str().parse::<TaskId>().unwrap(), id);
    }

    #[test]
    fn text_that_is_not_an_id_is_refused() {
        for bad in ["", "abc", "abcde", "ABCD", "ab-d", "abio"] {
            assert!(bad.parse::<TaskId>().is_err(), "`{bad}` must not parse");
        }
    }

    #[test]
    fn a_state_round_trips_through_its_text() {
        for state in TaskState::ALL {
            assert_eq!(state.as_str().parse::<TaskState>().unwrap(), state);
        }
        assert!("done".parse::<TaskState>().is_err());
    }

    /// The agenda is the union of three different reasons a task is today's business, and a
    /// date that has not arrived is none of them.
    #[test]
    fn a_task_is_todays_business_for_three_reasons_and_no_others() {
        let today = date(2026, 8, 19);
        let id = TaskId::mint("x", &BTreeSet::new());

        let committed = Task::new(id.clone(), "t", TaskState::Today, today);
        assert!(committed.is_active_on(today));

        let mut due = Task::new(id.clone(), "t", TaskState::Next, today);
        due.due = Some(today);
        assert!(due.is_active_on(today));
        due.due = Some(date(2026, 8, 20));
        assert!(!due.is_active_on(today), "a due date ahead is not today's");

        let mut waiting = Task::new(id.clone(), "t", TaskState::Waiting, today);
        assert!(
            !waiting.is_active_on(today),
            "waiting on nothing dated is not today's"
        );
        waiting.wake = Some(today);
        assert!(waiting.is_active_on(today));
        waiting.wake = Some(date(2026, 8, 20));
        assert!(!waiting.is_active_on(today));

        assert!(!Task::new(id, "t", TaskState::Someday, today).is_active_on(today));
    }
}
