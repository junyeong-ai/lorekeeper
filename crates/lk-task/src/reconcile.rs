use crate::board::Board;
use crate::log::{Recorded, Transition, TransitionKind};
use crate::task::{Task, TaskId, TaskState};

/// What a pass over the board changed, and the history it owes the log.
///
/// The transitions are RETURNED rather than written here, so the rules are a pure function of
/// the board and the clock — the part worth exhausting with tests — and the one place that
/// touches the filesystem is the command that calls it.
#[derive(Debug, Default)]
pub struct Reconciled {
    pub transitions: Vec<Transition>,
    /// Lines typed in an editor that this pass gave an address.
    pub adopted: Vec<TaskId>,
    /// Tasks checked off in an editor that this pass closed.
    pub harvested: Vec<TaskId>,
    /// Waiting tasks whose day arrived.
    pub woken: Vec<TaskId>,
    /// Tasks a day-close carried into the next one.
    pub carried: Vec<TaskId>,
    /// Ticked lines this pass removed whose completion the day's record already held — a board
    /// write that did not land, brought back into agreement without recording it twice.
    pub settled: Vec<TaskId>,
}

impl Reconciled {
    /// Whether this pass took `id` off the board for being ticked.
    ///
    /// Harvested and settled alike: the two differ in whether the completion was RECORDED here
    /// or already stood in the history, and to someone who read that id off the board a second
    /// ago they are one thing — the task is finished and the line is gone.
    pub fn closed(&self, id: &TaskId) -> bool {
        self.harvested.contains(id) || self.settled.contains(id)
    }

    /// What an EDITOR did, which is what a reader can tell someone to record.
    ///
    /// A wake is the clock arriving, not a person typing, so counting it here would report a
    /// board nobody touched as having unrecorded edits — which the shipped agenda snapshot did.
    pub fn edits(&self) -> usize {
        self.adopted.len() + self.harvested.len()
    }
}

impl Reconciled {
    pub fn is_empty(&self) -> bool {
        let Self {
            transitions,
            adopted: _,
            harvested: _,
            woken: _,
            carried: _,
            settled,
        } = self;
        transitions.is_empty() && settled.is_empty()
    }
}

/// Bring the board back into agreement with itself after it was edited somewhere else.
///
/// Three things can have happened in an editor since the last pass, and each has exactly one
/// correct answer: a checkbox line typed without a stamp is a task and is given an address; a
/// line checked off is a completion and is closed; a waiting task whose day has come is due
/// back. Everything else on the page is somebody's and is left alone.
///
/// Run before every mutation, so a command never acts on a board that has moved underneath it,
/// and by the day-close, so an editor-only user is fully served without ever typing a command.
pub fn sync(
    board: &mut Board,
    now: jiff::Timestamp,
    today: jiff::civil::Date,
    recorded: &Recorded,
) -> Reconciled {
    let mut outcome = Reconciled::default();

    // Adopting comes first so a line typed AND ticked in one sitting is created and then
    // completed, rather than being harvested as a task that never existed.
    // Minted against the history's ids as well as the board's: an id freed by a completion can
    // otherwise be minted again the same day, and a date's record holds one completion per task,
    // so the earlier task's would be overwritten by the later one and never reach the archive.
    let mut taken = board.ids();
    taken.extend(recorded.seen().cloned());
    for task in board.adopt_unstamped(|state, title, done| {
        let id = TaskId::mint(&format!("{title}{now}"), &taken);
        taken.insert(id.clone());
        let mut adopted = Task::new(id, title, state, today);
        adopted.done = done;
        adopted
    }) {
        outcome.transitions.push(
            Transition::new(task.id.clone(), TransitionKind::Created, &task.title, now)
                .with_state(task.state),
        );
        outcome.adopted.push(task.id);
    }

    // A ticked box is taken off the board either way. Whether it is also RECORDED depends on
    // whether the day already holds its completion: a board write that did not land must not
    // put a second finished task into the history — but leaving the line was worse than the
    // duplicate it avoided. The board and the history then disagreed until midnight, `list` and
    // `agenda` kept reporting a finished task as open, and once the date turned the guard could
    // no longer see the record at all, so the next pass harvested it again and the completion
    // was archived on two different days, which two `EventId`s cannot collapse.
    let checked: Vec<Task> = board.tasks().filter(|task| task.done).cloned().collect();
    for task in checked {
        board.remove(&task.id);
        if recorded.is_closed(&task.id) {
            outcome.settled.push(task.id);
            continue;
        }
        outcome.transitions.push(
            Transition::new(task.id.clone(), TransitionKind::Done, &task.title, now)
                .with_carried(task.carried),
        );
        outcome.harvested.push(task.id);
    }

    let woken: Vec<Task> = board
        .tasks()
        .filter(|task| {
            task.state == TaskState::Waiting && task.wake.is_some_and(|day| day <= today)
        })
        .cloned()
        .collect();
    for task in woken {
        // The wake date is cleared by arriving: it was a promise to resurface once, and a task
        // that keeps one would be woken again by every later pass.
        if let Some(moved) = board.tasks_mut().find(|open| open.id == task.id) {
            moved.wake = None;
        }
        let _ = board.move_to(&task.id, TaskState::Today);
        outcome.transitions.push(
            Transition::new(task.id.clone(), TransitionKind::Moved, &task.title, now)
                .with_state(TaskState::Today),
        );
        outcome.woken.push(task.id);
    }

    outcome
}

/// Close the day: every task still committed to it is carried into the next one, visibly.
///
/// Carrying is where most lists quietly fail — a task rolls forward untouched and forever, and
/// the roll is invisible because nothing counts it. Counting it turns a stale task into a
/// reading: one carried five times is not a task that needs another day, it is one that is too
/// large or was never real, and the count is what says so.
pub fn rollover(
    board: &mut Board,
    now: jiff::Timestamp,
    today: jiff::civil::Date,
    closing: jiff::civil::Date,
    recorded: &Recorded,
) -> Reconciled {
    // What the day was committed to BEFORE this pass touched it. A task the same pass woke or
    // adopted arrived today; stamping it `carried:1` would have it claim to have survived a
    // day-close its own `since` says it never saw.
    let began_with: std::collections::BTreeSet<TaskId> = board
        .tasks()
        .filter(|task| task.state == TaskState::Today)
        .map(|task| task.id.clone())
        .collect();

    let mut outcome = sync(board, now, today, recorded);

    // A day closes once, and WHICH day is the caller's to declare. Asking the record of the day
    // the command RAN could not answer it: a close by hand at 23:00 and the scheduled close at
    // 07:00 are two closes of one ended day whose records live in two different date files, so
    // each read the other's as empty and the count went up twice. The day is stamped on the
    // task, so the question is asked of the task itself. The day's record still answers the
    // within-a-day half, which is what lets a run that stopped partway resume.
    let committed: Vec<TaskId> = board
        .tasks()
        .filter(|task| task.state == TaskState::Today)
        .filter(|task| began_with.contains(&task.id))
        .filter(|task| task.carried_on != Some(closing))
        .filter(|task| !recorded.is_carried(&task.id))
        .map(|task| task.id.clone())
        .collect();

    for id in committed {
        let Some(task) = board.tasks_mut().find(|open| open.id == id) else {
            continue;
        };
        task.carried += 1;
        task.carried_on = Some(closing);
        let carried = task.carried;
        let title = task.title.clone();
        outcome.transitions.push(
            Transition::new(id.clone(), TransitionKind::Carried, title, now).with_carried(carried),
        );
        outcome.carried.push(id);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use lk_core::i18n::Locale;

    fn today() -> jiff::civil::Date {
        jiff::civil::date(2026, 8, 19)
    }

    fn now() -> jiff::Timestamp {
        today()
            .at(9, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    fn board_of(body: &str) -> Board {
        Board::parse(body)
    }

    #[test]
    fn a_line_typed_in_an_editor_is_given_an_address() {
        let mut board = board_of("## Today\n\n- [ ] wrote this in Obsidian\n");
        let outcome = sync(&mut board, now(), today(), &Recorded::from_day(&[]));

        assert_eq!(outcome.adopted.len(), 1);
        let task = board.tasks().next().expect("adopted");
        assert_eq!(task.title, "wrote this in Obsidian");
        assert_eq!(task.state, TaskState::Today);
        assert_eq!(task.since, today());
        assert_eq!(outcome.transitions[0].kind, TransitionKind::Created);
    }

    #[test]
    fn a_box_ticked_in_an_editor_closes_the_task() {
        let mut board = board_of("## Today\n\n- [x] a <!--t:7k2p since:2026-08-17 carried:2-->\n");
        let outcome = sync(&mut board, now(), today(), &Recorded::from_day(&[]));

        assert_eq!(board.tasks().count(), 0, "a closed task leaves the board");
        assert_eq!(outcome.harvested.len(), 1);
        let transition = &outcome.transitions[0];
        assert_eq!(transition.kind, TransitionKind::Done);
        assert_eq!(transition.carried, 2, "the archive learns how long it took");
        assert!(transition.observation().is_some());
    }

    /// Typing a line and ticking it in one sitting records both facts: the task existed, and it
    /// was finished. Harvesting first would close a task nothing had created.
    #[test]
    fn a_line_typed_and_ticked_at_once_is_created_then_closed() {
        let mut board = board_of("## Today\n\n- [x] did this before writing it down\n");
        let outcome = sync(&mut board, now(), today(), &Recorded::from_day(&[]));

        let kinds: Vec<TransitionKind> = outcome.transitions.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [TransitionKind::Created, TransitionKind::Done]);
        assert_eq!(board.tasks().count(), 0);
    }

    #[test]
    fn a_waiting_task_comes_back_on_the_day_it_named() {
        let mut board =
            board_of("## Waiting\n\n- [ ] a <!--t:7k2p since:2026-08-14 wake:2026-08-19-->\n");
        let outcome = sync(&mut board, now(), today(), &Recorded::from_day(&[]));

        let task = board.tasks().next().expect("still open");
        assert_eq!(task.state, TaskState::Today);
        assert_eq!(task.wake, None, "a promise kept is not kept again");
        assert_eq!(outcome.woken.len(), 1);
    }

    /// A wake date is inert until it arrives — nothing about a task is acted on ahead of its day.
    #[test]
    fn a_waiting_task_whose_day_has_not_come_is_untouched() {
        let mut board =
            board_of("## Waiting\n\n- [ ] a <!--t:7k2p since:2026-08-14 wake:2026-08-22-->\n");
        let outcome = sync(&mut board, now(), today(), &Recorded::from_day(&[]));

        assert_eq!(board.tasks().next().unwrap().state, TaskState::Waiting);
        assert!(outcome.is_empty());
    }

    #[test]
    fn a_board_that_changed_nowhere_else_reconciles_to_nothing() {
        let mut board = board_of("## Today\n\n- [ ] a <!--t:7k2p since:2026-08-19-->\n");
        let before = board.render(Locale::En, today());
        let outcome = sync(&mut board, now(), today(), &Recorded::from_day(&[]));

        assert!(outcome.is_empty());
        assert_eq!(board.render(Locale::En, today()), before);
    }

    /// The scheduled close and a close run by hand are the same day's. Counting the carry
    /// twice would inflate the only diagnostic this board offers.
    #[test]
    fn a_day_that_already_closed_is_not_closed_again() {
        let page = "## Today\n\n- [ ] a <!--t:7k2p since:2026-08-17 carried:3-->\n";
        let mut board = board_of(page);
        let first = rollover(
            &mut board,
            now(),
            today(),
            today(),
            &Recorded::from_day(&[]),
        );
        assert_eq!(board.get(&"7k2p".parse().unwrap()).unwrap().carried, 4);

        let second = rollover(
            &mut board,
            now(),
            today(),
            today(),
            &Recorded::from_day(&first.transitions),
        );
        assert!(second.carried.is_empty(), "the day closed once");
        assert_eq!(board.get(&"7k2p".parse().unwrap()).unwrap().carried, 4);
    }

    /// A run that stopped partway resumes exactly where it stopped, because the question is
    /// asked of each task's own record rather than of a marker for the whole day.
    #[test]
    fn a_close_that_stopped_partway_carries_only_what_it_missed() {
        let page = "## Today\n\n- [ ] a <!--t:7k2p since:2026-08-19-->\n- [ ] b <!--t:3b8q since:2026-08-19-->\n";
        let mut board = board_of(page);
        let recorded = [Transition::new(
            "7k2p".parse().unwrap(),
            TransitionKind::Carried,
            "a",
            now(),
        )];
        let outcome = rollover(
            &mut board,
            now(),
            today(),
            today(),
            &Recorded::from_day(&recorded),
        );
        assert_eq!(outcome.carried, vec!["3b8q".parse::<TaskId>().unwrap()]);
    }

    /// A task the same pass woke or adopted arrived today. Stamping it `carried:1` would have
    /// it claim to have survived a day-close its own `since` says it never saw.
    #[test]
    fn a_task_that_arrived_during_the_close_is_not_carried_by_it() {
        let woken = "## Waiting\n\n- [ ] a <!--t:7k2p since:2026-08-14 wake:2026-08-19-->\n";
        let mut board = board_of(woken);
        let outcome = rollover(
            &mut board,
            now(),
            today(),
            today(),
            &Recorded::from_day(&[]),
        );
        assert_eq!(outcome.woken.len(), 1);
        assert!(outcome.carried.is_empty(), "it spent the day waiting");
        assert_eq!(board.get(&"7k2p".parse().unwrap()).unwrap().carried, 0);

        let mut typed = board_of("## Today\n\n- [ ] written just now\n");
        let adopted = rollover(
            &mut typed,
            now(),
            today(),
            today(),
            &Recorded::from_day(&[]),
        );
        assert_eq!(adopted.adopted.len(), 1);
        assert!(adopted.carried.is_empty(), "it was written down today");
    }

    /// A completion the day already holds is a board write that did not land, not a second
    /// completion — but the LINE still has to go, or the board and the history disagree until
    /// midnight and the next day's pass harvests it again onto a second date, which two
    /// `EventId`s cannot collapse.
    #[test]
    fn a_completion_the_day_already_holds_settles_the_board_without_recording_it() {
        let page = "## Today\n\n- [x] a <!--t:7k2p since:2026-08-19-->\n";
        let mut board = board_of(page);
        let first = sync(&mut board, now(), today(), &Recorded::from_day(&[]));
        assert_eq!(first.harvested.len(), 1);

        // The board write failed, so the tick is still there when the next pass reads it.
        let mut retried = board_of(page);
        let second = sync(
            &mut retried,
            now(),
            today(),
            &Recorded::from_day(&first.transitions),
        );
        assert!(second.harvested.is_empty(), "not recorded twice");
        assert!(second.transitions.is_empty());
        assert_eq!(second.settled, vec!["7k2p".parse::<TaskId>().unwrap()]);
        assert_eq!(retried.tasks().count(), 0, "and the line is gone");
        assert!(!second.is_empty(), "so the caller writes the board back");
    }

    /// Two closes of ONE ended day, on two calendar dates. The record of the day the command
    /// ran cannot answer it — the evening close wrote to yesterday's file — so the day being
    /// closed is stamped on the task and the question is asked of the task.
    #[test]
    fn closing_one_ended_day_twice_counts_it_once() {
        let page = "## Today\n\n- [ ] a <!--t:7k2p since:2026-08-17-->\n";
        let mut board = board_of(page);
        let ended = today();

        let evening = rollover(&mut board, now(), ended, ended, &Recorded::from_day(&[]));
        assert_eq!(evening.carried.len(), 1);
        assert_eq!(board.get(&"7k2p".parse().unwrap()).unwrap().carried, 1);

        // The next morning: a different date, so a different record — and empty.
        let tomorrow = jiff::civil::date(2026, 8, 20);
        let morning = rollover(&mut board, now(), tomorrow, ended, &Recorded::from_day(&[]));
        assert!(
            morning.carried.is_empty(),
            "the same day does not close twice"
        );
        assert_eq!(board.get(&"7k2p".parse().unwrap()).unwrap().carried, 1);

        // And the day that follows still closes on its own terms.
        let next = rollover(
            &mut board,
            now(),
            tomorrow,
            tomorrow,
            &Recorded::from_day(&[]),
        );
        assert_eq!(next.carried.len(), 1);
        assert_eq!(board.get(&"7k2p".parse().unwrap()).unwrap().carried, 2);
    }

    #[test]
    fn a_day_close_carries_what_is_still_committed_and_counts_it() {
        let mut board = board_of(
            "## Today\n\n- [ ] a <!--t:7k2p since:2026-08-17 carried:3-->\n\
             ## Next\n\n- [ ] b <!--t:3b8q since:2026-08-18-->\n",
        );
        let outcome = rollover(
            &mut board,
            now(),
            today(),
            today(),
            &Recorded::from_day(&[]),
        );

        let carried = board.get(&"7k2p".parse().unwrap()).unwrap();
        assert_eq!(carried.carried, 4);
        let parked = board.get(&"3b8q".parse().unwrap()).unwrap();
        assert_eq!(
            parked.carried, 0,
            "only what was committed to the day is carried"
        );
        assert_eq!(outcome.carried, vec!["7k2p".parse::<TaskId>().unwrap()]);
    }

    /// The close is also a reconcile, so an editor-only user is served without ever running a
    /// command: what they ticked during the day is harvested by the same unattended pass.
    #[test]
    fn a_day_close_harvests_before_it_carries() {
        let mut board = board_of(
            "## Today\n\n- [x] finished <!--t:7k2p since:2026-08-19-->\n\
             - [ ] not yet <!--t:3b8q since:2026-08-19-->\n",
        );
        let outcome = rollover(
            &mut board,
            now(),
            today(),
            today(),
            &Recorded::from_day(&[]),
        );

        assert_eq!(outcome.harvested.len(), 1);
        assert_eq!(outcome.carried.len(), 1);
        assert_eq!(board.tasks().count(), 1);
        assert_eq!(board.get(&"3b8q".parse().unwrap()).unwrap().carried, 1);
    }

    /// A closed task must not also be carried; a carry is a statement that the day ended with
    /// it still open.
    #[test]
    fn a_task_closed_on_the_day_is_not_carried_into_the_next() {
        let mut board = board_of("## Today\n\n- [x] a <!--t:7k2p since:2026-08-19-->\n");
        let outcome = rollover(
            &mut board,
            now(),
            today(),
            today(),
            &Recorded::from_day(&[]),
        );
        assert!(outcome.carried.is_empty());
        assert!(
            outcome
                .transitions
                .iter()
                .all(|t| t.kind != TransitionKind::Carried)
        );
    }

    /// Two lines with the same text added at the same instant still get two addresses.
    #[test]
    fn two_identical_lines_are_two_tasks() {
        let mut board = board_of("## Today\n\n- [ ] same\n- [ ] same\n");
        sync(&mut board, now(), today(), &Recorded::from_day(&[]));
        let ids: Vec<&TaskId> = board.tasks().map(|task| &task.id).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    /// A line this could not read is not a line to adopt: re-stamping it would mint a second
    /// address for a task that already has one, and the two would then both be live.
    #[test]
    fn a_malformed_line_is_left_exactly_as_it_was() {
        let mut board = board_of("## Today\n\n- [ ] a <!--t:7k2p since:broken-->\n");
        let outcome = sync(&mut board, now(), today(), &Recorded::from_day(&[]));
        assert!(outcome.is_empty());
        assert_eq!(board.malformed().len(), 1);
        assert!(
            board.render(Locale::En, today()).contains("since:broken"),
            "the line survives untouched"
        );
    }
}
