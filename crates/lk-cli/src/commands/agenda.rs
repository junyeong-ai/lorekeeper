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
    match lk_task::Schedule::new(&plane.vault_root).read(plane.today, &plane.zone) {
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
                "  {}  {}  {}",
                task.id,
                super::pad(&lk_core::link::strip_links(&task.title), 46),
                annotation(task, &plane)
            );
        }
    }

    if committed.is_empty() && woken.is_empty() && due.is_empty() {
        eprintln!("\n{}", strings.agenda_empty);
    }

    match plane.closed_on(plane.today) {
        Ok(closed) if !closed.is_empty() => {
            eprintln!("\n{} {}", strings.agenda_done_today, closed.len())
        }
        Ok(_) => {}
        Err(e) => eprintln!("\nwarning: today's record could not be read ({e})"),
    }

    match plane.unrecorded(actually_today) {
        Some(0) => {}
        Some(n) => eprintln!("\n{n} change(s) made in an editor — `lore task sync` records them"),
        None => {
            eprintln!("\nwarning: the task record could not be read — `lore task sync` says why")
        }
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
/// task came from is reading prose again. What the view could not SEE is here too: a store it
/// could not read answers `null` rather than empty, and a task on the page that no section could
/// place is named with its line and its reason — a caller reading only the sections would report
/// a clean day over work sitting in plain sight in the person's editor.
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
            // The JUDGMENT the terminal shows, not the number it is computed from. The
            // threshold lives in `config.yaml`, which a skill running on `Bash(lore *)` cannot
            // read and no command prints — so shipping the count alone asked a caller to apply
            // a rule it had no way to know, which is the aligned-columns mistake in another key.
            "carried_too_long": task.carried >= plane.config.carry_warn_after,
            "overdue": task.is_overdue_on(plane.today),
            // Ticked in an editor and not yet recorded. Without it the contract said a
            // finished task was today's business — and where its completion is already in the
            // history, `unrecorded` is 0, so nothing else in the document said otherwise.
            "done": task.done,
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

    let schedule = read_or_null(
        lk_task::Schedule::new(&plane.vault_root).read(plane.today, &plane.zone),
        "the day's schedule",
        |appointment| {
            serde_json::json!({
                "at": appointment
                    .at
                    .to_zoned(plane.zone.clone())
                    .strftime("%FT%H:%M")
                    .to_string(),
                "title": appointment.title,
            })
        },
    );

    let reminders = read_or_null(
        lk_task::Reminders::new(&plane.vault_root).read(),
        "the standing reminders",
        |reminder| {
            serde_json::json!({
                "id": reminder.id.as_str(),
                "at": reminder.at.to_zoned(plane.zone.clone()).strftime("%FT%H:%M").to_string(),
                "text": reminder.text,
                "task": reminder.task.as_ref().map(|id| id.as_str().to_string()),
            })
        },
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "date": plane.today.to_string(),
            // The zone every time in this document is a wall-clock reading in, so a caller can
            // resolve one without asking the machine — whose own zone is a different day
            // whenever the vault's is set elsewhere. Its NAME, or its offset where it has no
            // name — a `TZ` in POSIX offset form has none, and answering `null` there would say
            // "could not read", which is what `null` means everywhere else in this document.
            "timezone": plane
                .zone
                .iana_name()
                .map(str::to_string)
                .unwrap_or_else(|| plane.zone.to_offset(plane.now).to_string()),
            "schedule": schedule,
            "committed": committed,
            "woken": woken,
            "due": due,
            "proposed": proposed,
            "reminders": reminders,
            // What was finished, not how much. "오늘 뭐 했지" and closing the day are the
            // second joint's whole point, and a bare count answered neither — the notes are
            // what reach the archive, and nothing else on this contract could reach them.
            "done_today": read_or_null(
                plane.closed_on(plane.today),
                "today's record",
                |transition| {
                    serde_json::json!({
                        "id": transition.id.as_str(),
                        "title": lk_core::link::strip_links(&transition.title),
                        "note": transition.note,
                        "carried": transition.carried,
                    })
                },
            ),
            // What an editor changed that no pass has recorded — the one thing this view can
            // see and cannot fix. `null` where the record could not be read at all, which a
            // caller must not mistake for a board that is caught up.
            "unrecorded": plane.unrecorded(actually_today),
            // Why a write to the board will be REFUSED, where one will. `null` is the ordinary
            // answer. Without it the document could report a clean, caught-up day over a board
            // whose every mutation is turned away — two lines claiming one id is a sync client's
            // ordinary conflict, and the id it duplicates is PLACED, so it never reaches
            // `unplaced` and nothing else here said a word.
            "unwritable": plane.unwritable,
            // Tasks the page holds that no section could place. Empty means every checkbox on
            // the page reached a list; anything here means the day this document describes is
            // INCOMPLETE, and a caller reading only the sections above would report a clean
            // morning while the person's work sat in plain sight in their editor. The reason
            // travels, because it is what they act on and the line is where they will look.
            "unplaced": plane.board.unplaced().iter().map(|held| serde_json::json!({
                "line": held.line,
                "why": held.why,
            })).collect::<Vec<_>>(),
        }))
        .unwrap_or_else(|_| "{}".into())
    );
    Ok(())
}

/// What a store holds as JSON, or `null` plus a word about why.
///
/// `null` and `[]` are different answers and a session acts on the difference: an empty list is
/// "nothing is promised", `null` is "I could not read your promises". Answering `[]` for both is
/// the shape refused everywhere else here, and it reached three of the four things this document
/// reports while only `unrecorded` had been fixed. The reason goes to stderr, where it cannot
/// corrupt the contract on stdout.
fn read_or_null<T>(
    read: Result<Vec<T>, lk_task::TaskError>,
    what: &str,
    row: impl Fn(T) -> serde_json::Value,
) -> serde_json::Value {
    match read {
        Ok(held) => serde_json::Value::Array(held.into_iter().map(row).collect()),
        Err(e) => {
            eprintln!("warning: {what} could not be read ({e})");
            serde_json::Value::Null
        }
    }
}
