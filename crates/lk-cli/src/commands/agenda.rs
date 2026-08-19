//! `lore agenda` — the day, read off the board.
//!
//! A view rather than a page. Writing one would materialize a forward-looking document, which
//! is the single thing this vault's architecture forbids: a date after today is a forecast, and
//! a forecast becomes knowledge only by arriving. The board's own `Today` section is already
//! the durable rendering of what is committed; this is the terminal rendering of the same
//! truth, with the day's dates resolved against it.
//!
//! It does not write, including the reconcile every mutation runs. What an editor changed and
//! nothing has recorded yet is REPORTED instead, naming the command that records it — a view
//! that quietly rewrote the board would be a mutation wearing a reader's name.

use lk_task::TaskState;

use super::GlobalOptions;
use super::task::{IntentPlane, annotation};

pub async fn run(opts: &GlobalOptions, date: Option<String>) -> miette::Result<()> {
    let mut plane = IntentPlane::open(opts)?;
    let actually_today = plane.today;
    plane.today = super::parse_date(date.as_deref(), plane.today)?;

    let strings = plane.locale.strings();
    eprintln!("{}", plane.today);

    // One predicate decides what is today's business — committed to it, due by it, or parked
    // until a day that has come — and the grouping below is presentation over that answer. A
    // second set of conditions here would be a second definition, and the two would agree only
    // until either was edited.
    let (mut committed, mut woken, mut due) = (Vec::new(), Vec::new(), Vec::new());
    for task in plane
        .board
        .tasks()
        .filter(|task| task.is_active_on(plane.today))
    {
        match task.state {
            TaskState::Today => committed.push(task),
            TaskState::Waiting => woken.push(task),
            // `is_active_on` never answers true for a proposal, so this arm is unreachable —
            // named rather than folded into the one below it, because a proposal is not
            // something due and a later edit to that predicate must decide this again.
            TaskState::Proposed => {}
            TaskState::Next | TaskState::Someday => due.push(task),
        }
    }

    // The day's appointments, which the agenda REPORTS and the board never learns of: a
    // meeting is a time already committed to, not work to decide about. A view answers with
    // what it can see, so an unreadable record costs the section rather than the command.
    match lk_task::Schedule::new(&plane.vault_root).read(plane.today) {
        Ok(appointments) if !appointments.is_empty() => {
            eprintln!("\n{}", strings.agenda_schedule);
            for appointment in appointments {
                let clock = appointment
                    .at
                    .to_zoned(plane.zone.clone())
                    .strftime("%H:%M");
                eprintln!("  {clock}  {}", appointment.title);
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("warning: the day's schedule could not be read ({e})"),
    }

    // Proposals are listed apart from the day's business and last: they ask to be ANSWERED
    // rather than worked on, and putting them among what a person committed to would make the
    // day look fuller than they made it.
    let proposed: Vec<_> = plane
        .board
        .tasks()
        .filter(|task| task.state == TaskState::Proposed)
        .collect();

    for (heading, tasks) in [
        (strings.tasks_today, &committed),
        (strings.tasks_waiting, &woken),
        (strings.agenda_due, &due),
        (strings.agenda_proposed, &proposed),
    ] {
        if tasks.is_empty() {
            continue;
        }
        eprintln!("\n{heading}");
        for task in tasks.iter() {
            eprintln!(
                "  {}  {:<48}{}",
                task.id,
                lk_core::link::strip_links(&task.title),
                annotation(task, &plane)
            );
        }
    }

    if committed.is_empty() && woken.is_empty() && due.is_empty() {
        eprintln!("\n{}", strings.agenda_empty);
    }

    let closed = plane.closed_on(plane.today);
    if !closed.is_empty() {
        eprintln!("\n{} {}", strings.agenda_done_today, closed.len());
    }

    let unrecorded = plane.unrecorded(actually_today);
    if unrecorded > 0 {
        eprintln!("\n{unrecorded} change(s) made in an editor — `lore task sync` records them");
    }
    Ok(())
}
