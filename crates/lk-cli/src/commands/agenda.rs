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

pub async fn run(opts: &GlobalOptions, date: Option<String>, json: bool) -> miette::Result<()> {
    let mut plane = IntentPlane::open(opts)?;
    let actually_today = plane.today;
    plane.today = super::parse_date(date.as_deref(), plane.today)?;

    if json {
        return emit_json(&plane, actually_today);
    }

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
                "  {}  {}{}",
                task.id,
                super::pad(&lk_core::link::strip_links(&task.title), 48),
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

/// The same day, for something that is not a person.
///
/// The view an agent reads every turn, so it is a CONTRACT rather than a rendering: the aligned
/// columns above exist to be scanned by eye, and asking a caller to parse them is the same
/// mistake `lore queue count` and `lore config vault-root` exist to prevent. Every section the
/// terminal shows is here, with the dates unformatted and the origin URL a proposal came from
/// carried through — an agent that has to strip a markdown link out of a title to know where a
/// task came from is reading prose again.
fn emit_json(plane: &IntentPlane, actually_today: jiff::civil::Date) -> miette::Result<()> {
    let task_json = |task: &lk_task::Task| {
        serde_json::json!({
            "id": task.id.as_str(),
            "title": lk_core::link::strip_links(&task.title),
            "state": task.state.as_str(),
            "since": task.since.to_string(),
            "due": task.due.map(|d| d.to_string()),
            "wake": task.wake.map(|d| d.to_string()),
            "carried": task.carried,
            "overdue": task.is_overdue_on(plane.today),
            "origin": lk_core::link::first_external_dest(&task.title),
        })
    };

    let mut committed = Vec::new();
    let mut woken = Vec::new();
    let mut due = Vec::new();
    let mut proposed = Vec::new();
    for task in plane.board.tasks() {
        match task.state {
            TaskState::Proposed => proposed.push(task_json(task)),
            _ if !task.is_active_on(plane.today) => {}
            TaskState::Today => committed.push(task_json(task)),
            TaskState::Waiting => woken.push(task_json(task)),
            TaskState::Next | TaskState::Someday => due.push(task_json(task)),
        }
    }

    let schedule: Vec<_> = lk_task::Schedule::new(&plane.vault_root)
        .read(plane.today)
        .unwrap_or_default()
        .into_iter()
        .map(|appointment| {
            serde_json::json!({
                "at": appointment
                    .at
                    .to_zoned(plane.zone.clone())
                    .strftime("%FT%H:%M")
                    .to_string(),
                "title": appointment.title,
            })
        })
        .collect();

    let reminders: Vec<_> = lk_task::Reminders::new(&plane.vault_root)
        .read()
        .unwrap_or_default()
        .into_iter()
        .map(|reminder| {
            serde_json::json!({
                "id": reminder.id.as_str(),
                "at": reminder.at.to_zoned(plane.zone.clone()).strftime("%FT%H:%M").to_string(),
                "text": reminder.text,
                "task": reminder.task.as_ref().map(|id| id.as_str().to_string()),
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "date": plane.today.to_string(),
            "schedule": schedule,
            "committed": committed,
            "woken": woken,
            "due": due,
            "proposed": proposed,
            "reminders": reminders,
            "done_today": plane.closed_on(plane.today).len(),
            // What an editor changed that no pass has recorded — the one thing this view can
            // see and cannot fix, so it names the command that can.
            "unrecorded": plane.unrecorded(actually_today),
        }))
        .unwrap_or_else(|_| "{}".into())
    );
    Ok(())
}
