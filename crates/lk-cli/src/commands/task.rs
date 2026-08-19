//! `lore task` — the intent plane's command surface.
//!
//! Every mutation follows one shape: read the board, reconcile whatever an editor did to it
//! since, apply the change, then write the board and record the history in the same breath.
//! Reconciling first is what keeps a command from acting on a board that has moved underneath
//! it — a box ticked on a phone an hour ago is a completion, and closing a second task without
//! noticing would write the board back with that one re-opened.

use std::fmt::Write as _;
use std::path::PathBuf;

use lk_core::config::{Config, TasksConfig};
use lk_core::i18n::Locale;
use lk_task::{Board, Task, TaskId, TaskState, Transition, TransitionKind, TransitionLog};

use super::{GlobalOptions, find_config, load_config};

#[derive(clap::Subcommand)]
pub enum TaskCommand {
    /// Write down something to do
    Add {
        /// What the task is. Taken as the rest of the line, so it needs no quoting.
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// Which section it lands in (default: next)
        #[arg(long, default_value = "next")]
        state: String,
        /// The day it is due (YYYY-MM-DD, or `today`/`tomorrow`)
        #[arg(long)]
        due: Option<String>,
        /// Where this came from — the Slack thread, the Jira issue, the mail, the meeting.
        ///
        /// An absolute URL, never a vault path. The task outlives the page it came from and is
        /// archived onto a different one, so a destination written relative to the board would
        /// resolve somewhere else from there; and a vault destination would make the board a
        /// citation source, so a concept's evidence would grow when the task was written and
        /// shrink when it was finished.
        #[arg(long)]
        link: Option<String>,
        /// What the link reads as (default: the locale's word for a source)
        #[arg(long)]
        label: Option<String>,
    },
    /// Show the board
    List {
        /// Only one section
        #[arg(long)]
        state: Option<String>,
        /// Emit the board as JSON
        #[arg(long)]
        json: bool,
    },
    /// Close a task
    Done {
        /// The task's id, as the board and `lore task list` print it
        id: String,
        /// What it taught. This becomes the archived page's body, and through it the concept
        /// extraction — which is how work performed compounds the way work read already does.
        #[arg(long)]
        note: Option<String>,
    },
    /// Take a task off the board without doing it
    Drop {
        /// The task's id, as the board and `lore task list` print it
        id: String,
    },
    /// Move a task to another section
    Move {
        /// The task's id, as the board and `lore task list` print it
        id: String,
        /// today, next, waiting or someday
        state: String,
    },
    /// Park a task until a day, then bring it back
    Wait {
        /// The task's id, as the board and `lore task list` print it
        id: String,
        /// The day it returns to today (YYYY-MM-DD, or `today`/`tomorrow`)
        #[arg(long)]
        until: String,
    },
    /// Say something at a time
    ///
    /// Kept off the board on purpose: a reminder is fired by a TIMER, and a timer that rewrites
    /// the board every few minutes writes a person's own file underneath an open editor and a
    /// sync client, forever. `wake` stays on the board because it is a state change; this only
    /// says something out loud.
    Remind {
        #[command(subcommand)]
        cmd: RemindCommand,
    },
    /// Name work an LLM session read out of a page, for the next `propose` to offer
    ///
    /// The judgment half of the first joint, and the only way in. A source's structured fields
    /// answer "is this unfinished" with no reading of prose; a mail does not, so the judgment
    /// is made where judgments are made and declared here as one. Refused for a source
    /// `personal.tasks.propose_from` does not name.
    Candidate {
        /// Which source's page this was read out of
        #[arg(long)]
        source: String,
        /// What the proposed line should read as
        #[arg(long)]
        summary: String,
        /// The absolute URL that addresses it
        #[arg(long)]
        url: String,
    },
    /// Put what the sources say is still open into the board's proposed section
    ///
    /// The intent plane's first joint. An observation PROPOSES and never creates: nothing here
    /// commits a task to a day, and the two answers a person can give already exist — drag the
    /// line into another section to accept it, `lore task drop` to decline.
    Propose,
    /// Record what an editor did: adopt lines typed by hand, close lines ticked, wake what is due
    Sync,
    /// Close the day — carry what is still committed to it, counting each carry
    Rollover {
        /// The day being closed — `yesterday`, `today`, or YYYY-MM-DD (default: today).
        ///
        /// Declared rather than inferred: a close run by hand in the evening and the scheduled
        /// one the next morning are two closes of ONE ended day, and nothing about the clock
        /// says so. The scheduled pipeline names `yesterday`, which `lore` resolves in
        /// `vault.timezone` — the zone every date here is derived in, and not the shell's.
        #[arg(long)]
        closing: Option<String>,
    },
}

#[derive(clap::Subcommand)]
pub enum RemindCommand {
    /// Promise to say something at a time
    Add {
        /// What to say
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
        /// When — `HH:MM` for today, or `YYYY-MM-DDTHH:MM`. Read in `vault.timezone`, which is
        /// the zone every other time here is derived in.
        #[arg(long)]
        at: String,
        /// The task this is about, where it is about one
        #[arg(long)]
        task: Option<String>,
    },
    /// What is promised and not yet said
    List {
        /// Emit them as JSON
        #[arg(long)]
        json: bool,
    },
    /// Take one back
    Drop {
        /// The reminder's id, as `add` and `list` print it
        id: String,
    },
    /// Print what is due now, and retire it
    ///
    /// The half a timer runs. What turns a line into a notification is the platform's, not
    /// this binary's — `lore-remind.sh` is the shipped one.
    Due,
}

pub async fn run(opts: &GlobalOptions, cmd: TaskCommand) -> miette::Result<()> {
    let mut plane = match cmd {
        TaskCommand::List { .. } => IntentPlane::open(opts)?,
        TaskCommand::Remind {
            cmd: RemindCommand::List { .. },
        } => IntentPlane::open(opts)?,
        _ => IntentPlane::open_for_change(opts)?,
    };

    match cmd {
        TaskCommand::List { state, json } => {
            let only = state.map(|s| parse_state(&s)).transpose()?;
            list(&plane, only, json);
            Ok(())
        }
        TaskCommand::Add {
            text,
            state,
            due,
            link,
            label,
        } => {
            let title = one_line(text.join(" "), TASK_ONE_LINE)?;
            let title = origin_title(title, link.as_deref(), label.as_deref(), &plane)?;
            let state = parse_state(&state)?;
            let due = due
                .as_deref()
                .map(|text| super::parse_date(Some(text), plane.today))
                .transpose()?;
            plane.reconcile()?;
            let id = TaskId::mint(&format!("{title}{}", plane.now), &plane.taken());
            let mut task = Task::new(id.clone(), &title, state, plane.today);
            task.due = due;
            // The same identity a proposal carries, so a task written by hand from a Jira issue
            // and one proposed from it are the same answer to that observation — and neither is
            // proposed again once it is finished or dropped.
            task.src = link.as_deref().map(lk_core::origin::identity);
            let src = task.src.clone();
            plane.board.insert(task);
            plane.record(
                Transition::new(id.clone(), TransitionKind::Created, &title, plane.now)
                    .with_state(state)
                    .with_src(src),
            );
            plane.commit().await?;
            eprintln!("{id}  {title}");
            // The id on STDOUT, alone, because the next thing a session does with a task it
            // just wrote down is address it — `--task <id>`, `move`, `done`. Left on stderr
            // beside the title, the only way to reach it was to scrape a rendering or re-read
            // the agenda and match on text, which is what every other machine contract here
            // exists to prevent.
            println!("{id}");
            Ok(())
        }
        TaskCommand::Done { id, note } => {
            let id = parse_id(&id)?;
            let pass = plane.reconcile()?;
            let title = match plane.annotate(&id, note.clone(), &pass.settled) {
                Some(title) => title,
                None => {
                    let task = plane.take(&id, &pass)?;
                    plane.record(
                        Transition::new(id.clone(), TransitionKind::Done, &task.title, plane.now)
                            .with_note(note)
                            .with_carried(task.carried)
                            .with_src(task.src.clone()),
                    );
                    task.title
                }
            };
            plane.commit().await?;
            eprintln!("done  {title}");
            Ok(())
        }
        TaskCommand::Drop { id } => {
            let id = parse_id(&id)?;
            let pass = plane.reconcile()?;
            let task = plane
                .board
                .remove(&id)
                .ok_or_else(|| plane.absent(&id, &pass))?;
            plane.record(
                Transition::new(id, TransitionKind::Dropped, &task.title, plane.now)
                    .with_src(task.src.clone()),
            );
            plane.commit().await?;
            eprintln!("dropped  {}", task.title);
            Ok(())
        }
        TaskCommand::Move { id, state } => {
            let id = parse_id(&id)?;
            let state = parse_state(&state)?;
            let pass = plane.reconcile()?;
            let moved = plane
                .board
                .move_to(&id, state)
                .map_err(|_| plane.absent(&id, &pass))?;
            // A wake date is a promise to resurface ONCE, and leaving Waiting is the resurfacing
            // — kept, it fires again from a section it means nothing in, and the agenda prints
            // "→ 2026-09-01" beside a task committed to today.
            if state != TaskState::Waiting
                && let Some(task) = plane.board.tasks_mut().find(|task| task.id == id)
            {
                task.wake = None;
            }
            plane.record(
                Transition::new(id, TransitionKind::Moved, &moved.title, plane.now)
                    .with_state(state),
            );
            plane.commit().await?;
            eprintln!("{state}  {}", moved.title);
            Ok(())
        }
        TaskCommand::Wait { id, until } => {
            let id = parse_id(&id)?;
            let until = super::parse_date(Some(&until), plane.today)?;
            let pass = plane.reconcile()?;
            let moved = plane
                .board
                .move_to(&id, TaskState::Waiting)
                .map_err(|_| plane.absent(&id, &pass))?;
            let title = moved.title.clone();
            if let Some(task) = plane.board.tasks_mut().find(|task| task.id == id) {
                task.wake = Some(until);
            }
            plane.record(
                Transition::new(id, TransitionKind::Moved, &title, plane.now)
                    .with_state(TaskState::Waiting),
            );
            plane.commit().await?;
            eprintln!("waiting until {until}  {title}");
            Ok(())
        }
        TaskCommand::Remind { cmd } => remind(&plane, cmd),
        TaskCommand::Candidate {
            source,
            summary,
            url,
        } => {
            if !plane.config.propose_from.contains(&source) {
                return Err(miette::miette!(
                    "`{source}` is not named by `personal.tasks.propose_from` — reading a \
                     source's prose for work is opt-in, because unlike a status field it can \
                     be wrong"
                ));
            }
            if !lk_core::link::is_external(&url) {
                return Err(miette::miette!(
                    "`{url}` is not an absolute URL — a proposal carries its origin onto the \
                     board and from there onto the archive page"
                ));
            }
            let candidate = lk_task::Candidate {
                source_id: source,
                summary: one_line(summary, TASK_ONE_LINE)?,
                url,
            };
            lk_task::Judged::new(&plane.vault_root)
                .add(plane.today, &candidate)
                .map_err(|e| miette::miette!("{e}"))?;
            eprintln!("noted  {}", candidate.summary);
            Ok(())
        }
        TaskCommand::Propose => {
            plane.reconcile()?;
            let pass = plane.propose()?;
            match pass.offered {
                0 => eprintln!("nothing new to propose"),
                n => eprintln!("{n} proposed"),
            }
            if pass.recoverable > 0 {
                eprintln!(
                    "{} finished earlier that the sources still call open — `lore task add \
                     --link <url>` writes one down again",
                    pass.recoverable
                );
            }
            plane.commit().await?;
            // Only once the board holds them. A failure here re-reads the same files next
            // time, where the board already names what they hold and nothing is offered twice.
            lk_task::Judged::retire(&pass.consumed).map_err(|e| miette::miette!("{e}"))
        }
        TaskCommand::Sync => {
            let outcome = plane.reconcile()?;
            report_reconcile(&outcome);
            plane.commit().await
        }
        TaskCommand::Rollover { closing } => {
            let closing = super::parse_date(closing.as_deref(), plane.today)?;
            // A day that has not ended cannot be closed. Left unbounded it stamped a future
            // `carried-on:` onto a managed page — the one thing a realized-only vault forbids —
            // and poisoned the guard for every close after it, since a later real day compares
            // as older.
            if closing > plane.today {
                return Err(miette::miette!(
                    "`{closing}` has not ended — a day is closed after it is over, and \
                     `{}` is the vault's today",
                    plane.today
                ));
            }
            let recorded = plane.read_history_from(plane.today, Some(closing))?;
            // `rollover` reconciles internally, so this path does not call `reconcile()`.
            let outcome =
                lk_task::rollover(&mut plane.board, plane.now, plane.today, closing, &recorded);
            report_reconcile(&outcome);
            plane.answered.extend(outcome.settled.iter().cloned());
            plane.pending.extend(outcome.transitions);
            plane.commit().await
        }
    }
}

/// The unplaced lines gathered by the reason they were unplaced, in file order.
///
/// The reason is what a person acts on, so lines sharing one are named together and each reason
/// is stated once. Its own message per line repeated the same sentence down the page whenever a
/// fence swallowed a section; one message for all of them made the reader work out which of the
/// several possible reasons was theirs.
fn group_by_reason(unplaced: &[lk_task::Unplaced]) -> Vec<(&str, String)> {
    let mut grouped: Vec<(&str, String)> = Vec::new();
    for held in unplaced {
        match grouped.iter_mut().find(|(why, _)| *why == held.why) {
            Some((_, lines)) => {
                let _ = write!(lines, ", L{}", held.line);
            }
            None => grouped.push((&held.why, format!("L{}", held.line))),
        }
    }
    grouped
}

/// What a proposal pass put on the board, and what it declined to.
struct Proposed {
    offered: usize,
    /// Origins the sources still call open that this pass will not offer, because the history
    /// holds a completion for them and no decision against.
    recoverable: usize,
    consumed: Vec<lk_task::Consumed>,
}

/// One line of the board's state, as `lore status` prints it.
pub(crate) struct BoardSurvey {
    pub(crate) state: String,
    /// Whether the front door's row prints `·` or `!`.
    ///
    /// The board is one of the rows whose command REPORTS rather than exits — `lore agenda` is
    /// a view and always succeeds — so this is not an exit code standing in for a verdict. It
    /// says a person has something to do here: a page a write cannot land on, or one holding
    /// tasks no rule can reach. Named for the write it once meant, it was the one field still
    /// running the two moments together after they were separated everywhere else.
    pub(crate) ok: bool,
}

/// The board, the log, and the clock the two are read against.
pub(crate) struct IntentPlane {
    pub(crate) board: Board,
    pub(crate) config: TasksConfig,
    pub(crate) locale: Locale,
    pub(crate) today: jiff::civil::Date,
    pub(crate) now: jiff::Timestamp,
    pub(crate) vault_root: PathBuf,
    board_path: PathBuf,
    pub(crate) zone: jiff::tz::TimeZone,
    /// The enabled sources this vault reads, so a snapshot left by one that was removed from
    /// the configuration cannot go on proposing work from a system nobody ingests any more.
    sources: Vec<String>,
    /// Why the board cannot be written, where it cannot. Reported when the plane is opened and
    /// refused when a write is attempted — never before.
    pub(crate) unwritable: Option<String>,
    /// Ids this pass took off the board without recording a transition — a ticked line whose
    /// completion the history already held. Answered as far as anything waiting on the task is
    /// concerned, and `pending` cannot say so.
    answered: Vec<TaskId>,
    pending: Vec<Transition>,
    /// The history this pass read. Kept because a command acting AFTER the reconcile asks the
    /// same question over the same window, and the board it was computed from — whose ticked
    /// lines bound that window — is gone by then.
    recorded: lk_task::Recorded,
    /// The page exactly as it was read, so a write can refuse to land on top of a version it
    /// never saw. `None` where there was no page yet.
    pub(crate) read_as: Option<Vec<u8>>,
    /// Held for a mutation, from before the board is read until after it and the log are
    /// written. Dropping it releases the plane.
    _guard: Option<lk_task::PlaneLock>,
}

impl IntentPlane {
    /// Read the board without claiming it. Every write goes through `write_atomic`, so a reader
    /// sees a whole file or the previous one, never a torn page — a reader has nothing to
    /// serialize against.
    pub(crate) fn open(opts: &GlobalOptions) -> miette::Result<Self> {
        Self::acquire(opts, false)
    }

    /// Claim the intent plane for a change.
    ///
    /// The plane, not the board. Every store beside it — the transition log, the proposal
    /// snapshots, the standing reminders — is a read-modify-write too, so two overlapping
    /// commands can both read, both write, and drop one of the two changes wherever they land.
    /// Scoping the lock to the board is what let `lore task candidate` and `lore task remind
    /// add` race each other on their own files: the guard was named for the first thing it
    /// happened to protect rather than for what it is protecting.
    ///
    /// The scheduled day-close and someone closing a task by hand is the ordinary collision,
    /// and the completion that disappears is gone from the history, which is the only thing the
    /// archive reads.
    ///
    /// The lock is the kernel's, so a crashed process releases it with no staleness rule to get
    /// wrong. It cannot help across machines — a vault synced by Dropbox or iCloud has two
    /// kernels — which is the same limitation an editor on either machine already has. Where it
    /// cannot be taken at all the command REFUSES: writing anyway lost board lines and history
    /// while reporting success.
    pub(crate) fn open_for_change(opts: &GlobalOptions) -> miette::Result<Self> {
        Self::acquire(opts, true)
    }

    fn acquire(opts: &GlobalOptions, exclusive: bool) -> miette::Result<Self> {
        let config = load_config(&find_config(opts)?)?;
        let tasks = tasks_config(&config)?;
        let vault_root = config.vault.root_path();
        let zone = config.vault.timezone();
        let now = jiff::Timestamp::now();
        let board_path =
            lk_core::vault_path::VaultPath::task_board(&config.vault.dirs, &tasks.board)
                .to_string();
        let full = vault_root.join(&board_path);

        // Claimed BEFORE the board is read, so the read and the write it decides are one
        // critical section rather than two.
        // A write claims the plane; a READ asks only whether one could be, which never waits
        // and never holds. Asked at all, because a plane nothing can hold refuses every write
        // while the board itself is perfectly readable — the front door printed a quiet day and
        // the contract said `unwritable: null` over a vault where the nightly close, every
        // command and the reminder timer were all being turned away.
        let mut guard = None;
        let mut unheld = None;
        match exclusive {
            true => {
                guard = Some(
                    lk_task::PlaneLock::hold(&vault_root).map_err(|e| miette::miette!("{e}"))?,
                )
            }
            false => {
                if let Err(why) = lk_task::PlaneLock::is_holdable(&vault_root) {
                    unheld = Some(why.to_string());
                }
            }
        }

        // An absent board is an empty one, not an error: the first `lore task add` creates the
        // page, exactly as the first ingest creates a daily page.
        let read_as = match std::fs::read(&full) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(miette::miette!("read {}: {e}", full.display())),
        };
        let board = match &read_as {
            Some(bytes) => Board::parse(&String::from_utf8_lossy(bytes)),
            None => Board::empty(),
        };
        // A page whose fence never closes is not a page this can write back. Everything below
        // that line is code to CommonMark, so the headings the render just emitted are read
        // back as code and a fresh set is emitted after them — a board that grows every night
        // A page a write cannot land on. Reported when the plane is OPENED and refused when a
        // write is attempted, which are two different moments: reading is always safe, and a
        // command that touches none of the board — a reminder firing, a judgment being noted —
        // has no business being turned away by a defect in a page it never opens. Coupled to
        // one flag, an unterminated fence silenced the reminder timer.
        //
        // An id addresses a task and every rule reaches one through it, so two lines answering
        // to one id make every rule ambiguous, and the ambiguity is settled by file order —
        // which deleted the wrong line in one arrangement and swallowed a completion in the
        // other. A fence that never closes makes every heading below it code. Both are
        // reported rather than repaired: which of two lines a person meant, and where they
        // meant a code block to end, are not knowable from the page.
        let mut unwritable = unheld;
        if !board.duplicated().is_empty() {
            let lines = board
                .duplicated()
                .iter()
                .map(|(id, line)| format!("L{line} (`{id}`)"))
                .collect::<Vec<_>>()
                .join(", ");
            // EVERY one of them, because a sync conflict duplicates a block rather than a line
            // and naming only the first would be repaired one line per run by anyone following
            // the message.
            unwritable = Some(format!(
                "{board_path}: {lines} carry an id an earlier line already claims — delete the \
                 `<!--t:…-->` comment from each copy and the next pass adopts it as its own task."
            ));
        }
        if let Some(line) = board.unterminated_fence() {
            unwritable = Some(format!(
                "{board_path} L{line} opens a code fence that nothing closes — every line below \
                 it is code, so the sections this reads and writes are not there. Close the \
                 fence with the same marker it opened with."
            ));
        }
        if let Some(notice) = &unwritable {
            eprintln!("warning: {notice}");
        }
        // A task on the page that the parse could not place is invisible to every rule while
        // sitting in plain sight in the editor. Grouped by REASON and never summarised away:
        // one sentence listing every way it can happen made the reader work out which of them
        // applied, and a line whose repair goes unsaid is one nobody repairs.
        for (why, lines) in group_by_reason(board.unplaced()) {
            eprintln!(
                "warning: {board_path} {lines} {} a task this cannot place — {why}. Until the \
                 line is repaired the task is in no list, no carry and no archive",
                if lines.contains(',') { "hold" } else { "holds" },
            );
        }

        Ok(Self {
            board,
            config: tasks,
            locale: config.vault.locale(),
            today: now.to_zoned(zone.clone()).date(),
            now,
            vault_root,
            board_path: PathBuf::from(board_path),
            zone,
            unwritable,
            answered: Vec::new(),
            sources: config
                .sources
                .iter()
                .filter(|(_, source)| source.enabled)
                .map(|(id, _)| id.clone())
                .collect(),
            pending: Vec::new(),
            recorded: lk_task::Recorded::default(),
            read_as,
            _guard: guard,
        })
    }

    /// Put every unanswered candidate into the proposed section, and answer with how many.
    ///
    /// Three things already settle whether a candidate has been dealt with, so a proposal needs
    /// no store of its own: it is on the BOARD (open, however it got there — proposed, accepted
    /// or written by hand with `--link`), or the HISTORY holds a completion or a drop for it,
    /// or the source no longer declares it at all, in which case it is not a candidate. What is
    /// left is work nobody has answered about, which is exactly what a proposal is.
    ///
    /// The one gap is a proposal deleted in an editor rather than dropped: nothing records
    /// that, so it returns tomorrow. That is the same silence deleting any task line already
    /// has, and a proposal that comes back is a smaller cost than one suppressed by a rule
    /// guessing at what a deletion meant.
    fn propose(&mut self) -> miette::Result<Proposed> {
        let snapshots = lk_task::Candidates::new(&self.vault_root)
            .read_all(&self.sources)
            .map_err(|e| miette::miette!("{e}"))?;
        let judged = lk_task::Judged::new(&self.vault_root)
            .take(&self.config.propose_from)
            .map_err(|e| miette::miette!("{e}"))?;
        for e in snapshots.unreadable.iter().chain(&judged.unreadable) {
            eprintln!("warning: {e}");
        }
        let consumed = judged.consumed;
        let mut candidates = snapshots.candidates;
        candidates.extend(judged.candidates);
        if candidates.is_empty() {
            return Ok(Proposed {
                offered: 0,
                recoverable: 0,
                consumed,
            });
        }

        let answered = TransitionLog::new(&self.vault_root)
            .answered_origins()
            .map_err(|e| miette::miette!("{e}"))?;
        // Every origin the PAGE answers to, including the ones on lines the parse could not
        // place: a proposal parked under a heading of the person's own, or ticked with a marker
        // this does not read, is still their answer sitting there, and offering it again every
        // morning is how the section stops being read.
        let standing = self.board.origins();

        // What the sources still declare, that this pass will not offer, and that a person
        // might want back. An origin is answered ONCE and for good, so an issue reopened weeks
        // later never returns on its own — reported as a quiet morning, every surface agreed the
        // board was empty while a source was actively declaring open work. Counted, it is a fact
        // they can act on.
        //
        // FINISHED and not since declined, never dropped: a drop is a decision that stands, and
        // naming it asks them to write down again what they deliberately said no to. Counted
        // together, the line grew with every correct use of `lore task drop` until the remedy it
        // named was wrong for most of what it was counting.
        let recoverable = candidates
            .iter()
            .map(lk_task::Candidate::origin)
            .filter(|origin| answered.is_recoverable(origin) && !standing.contains(origin))
            .collect::<std::collections::BTreeSet<_>>()
            .len();

        let offered = lk_task::select(candidates, &answered, &standing);
        for candidate in &offered {
            let origin = candidate.origin();
            let title = format!(
                "{} ({})",
                candidate.summary.trim(),
                lk_core::link::md_link(&candidate.source_id, &candidate.url)
            );
            let id = TaskId::mint(&format!("{origin}{}", self.now), &self.taken());
            let mut task = Task::new(id.clone(), &title, TaskState::Proposed, self.today);
            task.src = Some(origin.clone());
            self.board.insert(task);
            // A line on the board is a board write, and a board write without its transition is
            // something that happened and left no record — the rule every other route into
            // existence already follows. Without it the history could not say how a proposal
            // came to be there, and the id it holds was outside `Recorded::seen` for as long as
            // nobody answered it.
            self.record(
                Transition::new(id, TransitionKind::Created, &title, self.now)
                    .with_state(TaskState::Proposed)
                    .with_src(Some(origin)),
            );
        }
        Ok(Proposed {
            offered: offered.len(),
            recoverable,
            consumed,
        })
    }

    /// Whether `reminder` is about a task the board no longer holds open — `None` where the
    /// board cannot answer.
    ///
    /// It cannot answer when there is no page at all (renamed, moved, or a sync client's
    /// placeholder not yet materialized) or when a code fence that never closes has made every
    /// task invisible to the parse. In both, `tasks()` is empty and every task-linked reminder
    /// reads as moot — so a caller that acted on that would drop exactly the promises it exists
    /// to keep. A duplicated id is not one of these: both lines parse, so the question is still
    /// answerable.
    pub(crate) fn is_moot(&self, reminder: &lk_task::Reminder) -> Option<bool> {
        let Some(id) = reminder.task.as_ref() else {
            return Some(false);
        };
        // Finding it is PROOF and needs no guard. Only the negative depends on the parse having
        // been complete — which is one question rather than a list of the ways it can fail, and
        // the list was what let a per-line case walk past a whole-board guard.
        if self.board.tasks().any(|task| &task.id == id) {
            return Some(false);
        }
        if self.read_as.is_none() || !self.board.unplaced().is_empty() {
            return None;
        }
        Some(true)
    }

    /// One line of the board's state, for the front door.
    ///
    /// `None` where the intent plane is off — an install that never configured a board has no
    /// board row, the same way a vault with no `personal:` module has no work-log.
    ///
    /// Read WITHOUT claiming the plane and without reconciling: a front door that quietly
    /// rewrote the page a person is looking at would be a mutation wearing a reader's name,
    /// which is the same rule `lore agenda` follows.
    pub(crate) fn survey(opts: &GlobalOptions) -> Option<BoardSurvey> {
        let tasks = load_config(&find_config(opts).ok()?)
            .ok()?
            .personal
            .and_then(|personal| personal.tasks)?;
        let _ = tasks;
        // Past that point the plane is configured, so a failure to open it is a board this
        // cannot read — which must not read as an install that never turned the plane on.
        let plane = match Self::open(opts) {
            Ok(plane) => plane,
            Err(e) => {
                eprintln!("warning: {e}");
                return Some(BoardSurvey {
                    state: Locale::En.strings().status_board_unreadable.to_string(),
                    ok: false,
                });
            }
        };
        let strings = plane.locale.strings();
        // Asked of the one flag a write is refused on, rather than of the defects that set it:
        // a plane nothing can HOLD refuses every write too, and named defect by defect the row
        // reported a quiet day over a vault where the nightly close was being turned away.
        if plane.unwritable.is_some() {
            return Some(BoardSurvey {
                state: strings.status_board_unwritable.to_string(),
                ok: false,
            });
        }

        // A page holding tasks this cannot place reads as an empty board to every rule, so the
        // front door must not report it as a quiet day.
        if !plane.board.unplaced().is_empty() {
            return Some(BoardSurvey {
                state: strings.status_board_unplaceable.to_string(),
                ok: false,
            });
        }

        let count = |state| plane.board.tasks().filter(|t| t.state == state).count();
        let committed = count(TaskState::Today);
        let proposed = count(TaskState::Proposed);
        // Only what is committed to TODAY: a carry count is a diagnosis about a task being
        // asked for again, and one parked in Someday is not being asked for.
        let stale = plane
            .board
            .tasks()
            .filter(|task| {
                task.state == TaskState::Today && task.carried >= plane.config.carry_warn_after
            })
            .count();

        let mut state = format!("{committed} {}", strings.status_board_committed);
        if proposed > 0 {
            let _ = write!(state, " · {proposed} {}", strings.status_board_proposed);
        }
        if stale > 0 {
            let _ = write!(state, " · {stale} {}", strings.status_board_carried);
        }
        match plane.unrecorded(plane.today) {
            Some(0) => {}
            Some(n) => {
                let _ = write!(state, " · {n} {}", strings.status_board_unrecorded);
            }
            // The row a front door prints must not read the same whether the history is caught
            // up or unreadable, which is the difference the whole rule is about.
            None => {
                return Some(BoardSurvey {
                    state: strings.status_board_unreadable.to_string(),
                    ok: false,
                });
            }
        }
        Some(BoardSurvey { state, ok: true })
    }

    /// Every id a new task must not be given.
    ///
    /// The board's, the window's, and this pass's own — the third because `sync` takes the
    /// history by shared reference and so cannot record what it frees: an id harvested a moment
    /// ago is off the board and its completion is in `pending`, in no file the window read. A
    /// task minted onto it would put `Done(id, …)` and then `Created(id, …)` into one date's
    /// record, and the new task completing that same day would overwrite the first completion,
    /// which is the loss the window's ids are here to make unreachable rather than unlikely.
    fn taken(&self) -> std::collections::BTreeSet<TaskId> {
        let mut taken = self.board.ids();
        taken.extend(self.recorded.seen().cloned());
        taken.extend(self.pending.iter().map(|transition| transition.id.clone()));
        taken
    }

    fn reconcile(&mut self) -> miette::Result<lk_task::Reconciled> {
        self.recorded = self.read_history(self.today)?;
        let outcome = lk_task::sync(&mut self.board, self.now, self.today, &self.recorded);
        self.pending.extend(outcome.transitions.clone());
        self.answered.extend(outcome.settled.iter().cloned());
        Ok(outcome)
    }

    /// Attach a note to a completion this pass already recorded.
    ///
    /// Closing a task whose box was ticked in an editor first used to fail — the reconcile had
    /// harvested it, so `take` found nothing, and because the command then returned an error
    /// nothing was committed at all, discarding the harvest with it. The note is the one thing
    /// that carries a completion into concept extraction, and the only way to supply it must
    /// not be "do not tick the box first".
    fn annotate(
        &mut self,
        id: &TaskId,
        note: Option<String>,
        settled: &[TaskId],
    ) -> Option<String> {
        if let Some(recorded) = self
            .pending
            .iter_mut()
            .find(|transition| &transition.id == id && transition.kind == TransitionKind::Done)
        {
            if let Some(note) = note {
                *recorded = recorded.clone().with_note(Some(note));
            }
            return Some(recorded.title.clone());
        }
        // A line THIS PASS settled — a ticked box whose completion the history already held,
        // because the board write that should have removed it did not land. Answering with its
        // title closes the command cleanly instead of erroring on an id the user just read off
        // the board, and a second completion is what is refused, not the note: one task closes
        // once, so the sentence joins the completion already written rather than being dropped
        // in silence, exactly as it would have joined one this same run recorded.
        //
        // Asked of the id's HISTORY instead, it fired on a task the board still showed as OPEN
        // whenever some older completion sat in the window — an editor undo or a sync client
        // restoring an older page is enough — and the work a person did today was folded onto
        // that day instead of being closed on this one.
        if !settled.contains(id) {
            return None;
        }
        let closed = self.recorded.closure(id)?.clone();
        self.board.remove(id);
        let title = closed.title.clone();
        if note.is_some() {
            self.record(closed.with_note(note));
        }
        Some(title)
    }

    fn record(&mut self, transition: Transition) {
        self.pending.push(transition);
    }

    fn take(&mut self, id: &TaskId, pass: &lk_task::Reconciled) -> miette::Result<Task> {
        self.board.remove(id).ok_or_else(|| self.absent(id, pass))
    }

    /// Why no task answers to `id`.
    ///
    /// "No task answers to `7k2p`" is false in the one way that matters to a person who read
    /// that id off the board a second earlier: this pass took the line off it, because the
    /// history already held its completion. The command still fails — answering "moved" about a
    /// finished task would be a worse answer than an error — and the settle it discards is
    /// re-derived by the next pass from the same board and the same record.
    fn absent(&self, id: &TaskId, pass: &lk_task::Reconciled) -> miette::Report {
        if pass.closed(id) {
            return miette::miette!("`{id}` was already closed — this pass took it off the board");
        }
        self.unfound(id)
    }

    /// Why `id` was not found — which is an ANSWER only where the parse placed every task the
    /// page holds.
    ///
    /// Finding an id is proof on its own and needs no guard; not finding one means nothing where
    /// a line naming a task was dropped, and the id may be sitting on exactly that line. Stated
    /// flat, the message told a person their task does not exist while they were looking at it.
    /// `Board::unplaced` already answers this; two call sites asked the raw question.
    fn unfound(&self, id: &TaskId) -> miette::Report {
        match self.board.unplaced().first() {
            Some(held) => miette::miette!(
                "`{id}` is not among the tasks this could read — and L{} holds one it could not \
                 place ({}), which may be it. Repair that line and ask again",
                held.line,
                held.why
            ),
            None => miette::miette!("no task answers to `{id}`"),
        }
    }

    /// Write the board and the history it owes, history first.
    ///
    /// Order matters on a crash: a transition recorded without the board move is re-derived
    /// harmlessly by the next reconcile, while a board written without its transition is work
    /// that happened and left no record — and the log is the only thing the archive reads.
    async fn commit(&mut self) -> miette::Result<()> {
        // A page a write cannot land on is refused HERE, where the write is, and nowhere
        // earlier. Nothing has been written at this point, so a refusal leaves nothing
        // half-done — and a command that never reaches this line was never the board's
        // business to turn away.
        if let Some(notice) = &self.unwritable {
            return Err(miette::miette!("{notice}"));
        }

        // Asked BEFORE anything is written. The lock keeps two `lore task` runs apart, but an
        // editor or a sync client is not holding it, and this command's board was read before
        // theirs landed — writing it back would erase their edit wholesale. Checked ahead of
        // the log so a refusal leaves nothing recorded either, rather than a completion in the
        // history for a task still sitting on the board.
        let now_on_disk = match std::fs::read(self.vault_root.join(&self.board_path)) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(miette::miette!("re-read the board: {e}")),
        };
        if now_on_disk != self.read_as {
            return Err(miette::miette!(
                "{} changed while this command was running — nothing was written; run it again",
                self.board_path.display()
            ));
        }

        let mut answered: Vec<TaskId> = self
            .pending
            .iter()
            .filter(|transition| transition.kind.is_answer())
            .map(|transition| transition.id.clone())
            .collect();
        // A SETTLED line answers too. Its completion was recorded by an earlier pass, so this
        // one writes no transition — and reading only what this pass recorded left the reminder
        // standing for a task the board had just let go. `Reconciled::closed` exists to say
        // harvested and settled are one thing to anyone who was waiting on the task.
        answered.append(&mut self.answered);

        if !self.pending.is_empty() {
            TransitionLog::new(&self.vault_root)
                .record(&self.pending, &self.zone)
                .map_err(|e| miette::miette!("{e}"))?;
            self.pending.clear();
        }
        let page = self.board.render(self.locale, self.today);
        lk_vault::VaultWriter::new(&self.vault_root)
            .write_page(&self.board_path, &page)
            .await
            .map_err(|e| miette::miette!("write {}: {e}", self.board_path.display()))?;

        self.retire_reminders(&answered);
        Ok(())
    }

    /// Drop the reminders about tasks this pass ANSWERED.
    ///
    /// Done at the moment the task leaves the board rather than when the timer next looks,
    /// because `lore task remind list` is read by a session deciding what to tell someone, and a
    /// reminder about work already finished is a wrong answer sitting there until it fires. A
    /// notification telling a person to do what they did this morning is the one failure that
    /// makes them stop reading notifications.
    ///
    /// Best-effort and after the board write: a reminder that outlives its task costs one
    /// misfire, while refusing the whole command would undo a completion that already happened.
    fn retire_reminders(&self, answered: &[TaskId]) {
        if answered.is_empty() {
            return;
        }
        let store = lk_task::Reminders::new(&self.vault_root);
        let held = match store.read() {
            Ok(held) => held,
            Err(e) => {
                eprintln!("warning: the reminders could not be read ({e})");
                return;
            }
        };
        let moot: Vec<_> = held
            .into_iter()
            .filter(|reminder| {
                reminder
                    .task
                    .as_ref()
                    .is_some_and(|id| answered.contains(id))
            })
            .collect();
        for reminder in moot {
            match store.remove(&reminder.id) {
                // Said rather than done in silence: a promise the person made and this took
                // back is theirs to know about.
                Ok(true) => eprintln!("            reminder dropped — {}", reminder.text),
                Ok(false) => {}
                Err(e) => eprintln!("warning: {e}"),
            }
        }
    }

    /// What the history holds about the tasks on this board, over the window each of its two
    /// questions is answerable across.
    ///
    /// A day whose record will not read is fatal here rather than warned past: the completion
    /// guard reads several dates, and treating one unreadable file as an empty day would harvest
    /// a tick the missing half already recorded — a second copy of one completion, on a second
    /// date, which nothing downstream collapses. `lore ingest` fails on the same file, which is
    /// where the repair belongs.
    fn read_history(&self, on: jiff::civil::Date) -> miette::Result<lk_task::Recorded> {
        self.read_history_from(on, None)
    }

    /// The history, widened to reach `floor` — the day a close is closing, whose carry
    /// transition may have been written on any day since.
    fn read_history_from(
        &self,
        on: jiff::civil::Date,
        floor: Option<jiff::civil::Date>,
    ) -> miette::Result<lk_task::Recorded> {
        TransitionLog::new(&self.vault_root)
            .recorded_for(&self.board, on, floor)
            .map_err(|e| miette::miette!("{e}"))
    }

    pub(crate) fn recorded_on(
        &self,
        date: jiff::civil::Date,
    ) -> Result<Vec<Transition>, lk_task::TaskError> {
        let log = TransitionLog::new(&self.vault_root);
        // The whole store before the one day, because a name that is not a date is a conflict
        // copy holding completions this cannot read. Answered from the file it COULD read, the
        // day came back as a confident list that was missing them — on a store where every
        // write is already refused for exactly this reason.
        log.dates()?;
        log.read(date)
    }

    /// Completions already recorded for `date` — what the day has to show for itself.
    pub(crate) fn closed_on(
        &self,
        date: jiff::civil::Date,
    ) -> Result<Vec<Transition>, lk_task::TaskError> {
        Ok(self
            .recorded_on(date)?
            .into_iter()
            .filter(|transition| transition.kind.is_observation())
            .collect())
    }

    /// What an editor changed that no pass has recorded — `None` where the history cannot be
    /// read, which is not the same as nothing.
    ///
    /// Asked about TODAY, never about a day the caller is previewing: probing with an overridden
    /// date reported edits nobody made and named a command that then answered "nothing to
    /// record".
    ///
    /// A view answers with what it can SEE and says when it cannot see. Answering `0` on an
    /// unreadable record was the one shape this codebase refuses everywhere else: a caller
    /// reading it — the front door's board row, the JSON a session acts on — cannot tell "your
    /// board is caught up" from "I could not look", and the second is the one that needs saying.
    pub(crate) fn unrecorded(&self, on: jiff::civil::Date) -> Option<usize> {
        let mut probe = self.board.clone();
        // Said out loud, not only answered `None`. A caller reading a `null` knows it could not
        // look and cannot know at WHICH file to look — and the reason costs nothing to carry,
        // where deriving it a second time from the same store would be a second answer.
        let recorded = match self.read_history(on) {
            Ok(recorded) => recorded,
            Err(e) => {
                eprintln!("warning: the task record could not be read ({e})");
                return None;
            }
        };
        Some(lk_task::sync(&mut probe, self.now, on, &recorded).edits())
    }
}

fn tasks_config(config: &Config) -> miette::Result<TasksConfig> {
    config
        .personal
        .as_ref()
        .and_then(|personal| personal.tasks.clone())
        .ok_or_else(|| {
            miette::miette!(
                "no task board is configured — add a `tasks:` block under `personal:` in \
                 config.yaml, and a source with `type: tasks` for its completions to land on"
            )
        })
}

/// The task's own text, with where it came from appended as an ordinary markdown link.
///
/// In the VISIBLE half rather than the machine stamp, and absolute rather than vault-relative,
/// which is what makes it survive everything the task does: it is copied verbatim onto the
/// archive page when the task closes, it stays resolvable after the origin page is re-rendered
/// or was never written, and `lk_core::link::is_external` keeps it out of the link graph
/// entirely — no edge, no broken-link finding, and no citation whose evidence appears when a
/// task is written down and disappears when it is finished.
fn origin_title(
    text: String,
    link: Option<&str>,
    label: Option<&str>,
    plane: &IntentPlane,
) -> miette::Result<String> {
    let Some(url) = link else {
        return Ok(text);
    };
    if !lk_core::link::is_external(url) {
        return Err(miette::miette!(
            "`{url}` is not an absolute URL — a task is archived onto a different page than the \
             board, so a vault path written here resolves somewhere else from there"
        ));
    }
    let label = label.unwrap_or(plane.locale.strings().task_origin);
    Ok(format!("{text} ({})", lk_core::link::md_link(label, url)))
}

/// A task is one line, because its stamp sits at the end of that line.
///
/// A title carrying a newline split the stamp onto a line of its own, which the next read took
/// as ordinary content: the task became unreachable by its own id forever, a phantom was minted
/// from the first half, and the log held a `created` for an address no command could close.
/// Refuse text that would not survive being written on one line.
///
/// `why` is the caller's, because the reason differs and a wrong one sends someone looking in
/// the wrong place: a task line carries its stamp at the end, while a reminder is printed one
/// per line to whatever says it out loud.
fn one_line(text: String, why: &str) -> miette::Result<String> {
    if text.contains(['\n', '\r']) {
        return Err(miette::miette!("{why}"));
    }
    Ok(text)
}

const TASK_ONE_LINE: &str = "a task's text must be one line — its stamp sits at the end of that \
                             line, and a break would leave the stamp behind as ordinary content";

const REMINDER_ONE_LINE: &str = "a reminder must be one line — it is printed one per line to \
                                 whatever says it out loud, and a break would be said twice";

fn parse_state(text: &str) -> miette::Result<TaskState> {
    text.parse().map_err(|e| miette::miette!("{e}"))
}

fn parse_id(text: &str) -> miette::Result<TaskId> {
    text.parse().map_err(|e| miette::miette!("{e}"))
}

/// `lore task remind` — the promises, and the half a timer fires.
fn remind(plane: &IntentPlane, cmd: RemindCommand) -> miette::Result<()> {
    let store = lk_task::Reminders::new(&plane.vault_root);
    match cmd {
        RemindCommand::Add { text, at, task } => {
            let text = one_line(text.join(" "), REMINDER_ONE_LINE)?;
            let at = parse_moment(&at, plane)?;
            let task = task.as_deref().map(parse_id).transpose()?;
            if let Some(id) = &task
                && !plane.board.tasks().any(|open| &open.id == id)
            {
                return Err(plane.unfound(id));
            }
            let held = store.read().map_err(|e| miette::miette!("{e}"))?;
            let taken = held.iter().map(|r| r.id.clone()).collect();
            let id = TaskId::mint(&format!("{text}{at}"), &taken);
            store
                .add(lk_task::Reminder {
                    id: id.clone(),
                    at,
                    text: text.clone(),
                    task,
                })
                .map_err(|e| miette::miette!("{e}"))?;
            eprintln!(
                "{id}  {}  {text}",
                at.to_zoned(plane.zone.clone()).strftime("%F %H:%M")
            );
            Ok(())
        }
        RemindCommand::List { json } => {
            let held = store.read().map_err(|e| miette::miette!("{e}"))?;
            if json {
                let rows: Vec<_> = held
                    .iter()
                    .map(|reminder| {
                        serde_json::json!({
                            "id": reminder.id.as_str(),
                            "at": reminder.at.to_zoned(plane.zone.clone()).strftime("%FT%H:%M").to_string(),
                            "text": reminder.text,
                            "task": reminder.task.as_ref().map(|id| id.as_str()),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into())
                );
                return Ok(());
            }
            for reminder in held {
                eprintln!(
                    "{}  {}  {}",
                    reminder.id,
                    reminder
                        .at
                        .to_zoned(plane.zone.clone())
                        .strftime("%F %H:%M"),
                    reminder.text
                );
            }
            Ok(())
        }
        RemindCommand::Drop { id } => {
            let id = parse_id(&id)?;
            if !store.remove(&id).map_err(|e| miette::miette!("{e}"))? {
                return Err(miette::miette!("no reminder answers to `{id}`"));
            }
            eprintln!("dropped  {id}");
            Ok(())
        }
        RemindCommand::Due => {
            // To STDOUT, because this is the one output another program consumes — the same
            // contract `queue count` and `config vault-root` keep.
            //
            // A reminder about a task that is no longer OPEN is moot however the task left the
            // board — finished, dropped, or a line deleted in an editor, which records nothing
            // and so no completion could retire it. The board is the truth about what is open,
            // so it is what decides — and where it cannot decide, NOTHING is said and nothing is
            // retired. A promise this store guarantees is late is not one to lose to a page that
            // did not parse.
            let mut answered = Vec::new();
            for reminder in store.due(plane.now).map_err(|e| miette::miette!("{e}"))? {
                match plane.is_moot(&reminder) {
                    Some(false) => {
                        println!("{}", reminder.text);
                        answered.push(reminder.id);
                    }
                    Some(true) => answered.push(reminder.id),
                    None => {}
                }
            }
            store
                .answered(&answered)
                .map_err(|e| miette::miette!("{e}"))
        }
    }
}

/// A wall-clock moment in the VAULT's zone: `HH:MM` today, or `YYYY-MM-DDTHH:MM`.
///
/// Resolved here rather than taken as an instant, because every other time in this tool is
/// derived through `vault.timezone` and a reminder read in the machine's zone would fire an
/// hour out on a laptop that travelled.
fn parse_moment(text: &str, plane: &IntentPlane) -> miette::Result<jiff::Timestamp> {
    let civil: jiff::civil::DateTime = match text.split_once('T') {
        Some(_) => text.parse().map_err(|e| {
            miette::miette!("`{text}` is not a moment ({e}) — expected HH:MM or YYYY-MM-DDTHH:MM")
        })?,
        None => {
            let time: jiff::civil::Time = text.parse().map_err(|e| {
                miette::miette!("`{text}` is not a time ({e}) — expected HH:MM or YYYY-MM-DDTHH:MM")
            })?;
            plane.today.at(time.hour(), time.minute(), 0, 0)
        }
    };
    civil
        .to_zoned(plane.zone.clone())
        .map(|zoned| zoned.timestamp())
        .map_err(|e| {
            miette::miette!(
                "`{text}` does not exist in {}: {e}",
                plane.zone.iana_name().unwrap_or("the vault's zone")
            )
        })
}

fn list(plane: &IntentPlane, only: Option<TaskState>, json: bool) {
    if json {
        let rows: Vec<serde_json::Value> = plane
            .board
            .tasks()
            .filter(|task| only.is_none_or(|state| task.state == state))
            .map(|task| {
                serde_json::json!({
                    "id": task.id.as_str(),
                    "title": task.title,
                    "state": task.state.as_str(),
                    "since": task.since.to_string(),
                    "due": task.due.map(|d| d.to_string()),
                    "wake": task.wake.map(|d| d.to_string()),
                    "carried": task.carried,
                    "done": task.done,
                    "src": task.src,
                    "origin": lk_core::link::first_external_dest(&task.title),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".into())
        );
        return;
    }

    for state in TaskState::ALL {
        if only.is_some_and(|wanted| wanted != state) {
            continue;
        }
        let tasks: Vec<&Task> = plane
            .board
            .tasks()
            .filter(|task| task.state == state)
            .collect();
        if tasks.is_empty() && only.is_none() {
            continue;
        }
        eprintln!("\n{}", state.heading(plane.locale));
        for task in tasks {
            eprintln!(
                "  {}  {}  {}",
                task.id,
                super::pad(&lk_core::link::strip_links(&task.title), 46),
                annotation(task, plane)
            );
        }
    }
}

/// What a task's dates and carry count add to its line, in the vault's locale.
pub(crate) fn annotation(task: &Task, plane: &IntentPlane) -> String {
    let strings = plane.locale.strings();
    let mut parts = Vec::new();
    if let Some(due) = task.due {
        parts.push(if task.is_overdue_on(plane.today) {
            format!("{} {due}", strings.agenda_overdue)
        } else {
            format!("{} {due}", strings.agenda_due)
        });
    }
    if let Some(wake) = task.wake {
        parts.push(format!("→ {wake}"));
    }
    if task.carried >= plane.config.carry_warn_after {
        parts.push(format!("{}{}", task.carried + 1, strings.agenda_day));
    }
    parts.join("  ")
}

fn report_reconcile(outcome: &lk_task::Reconciled) {
    if outcome.is_empty() {
        eprintln!("nothing to record");
        return;
    }
    for (count, what) in [
        (outcome.adopted.len(), "adopted from the editor"),
        (outcome.harvested.len(), "closed"),
        (outcome.woken.len(), "woken"),
        (outcome.carried.len(), "carried into the next day"),
        (
            outcome.settled.len(),
            "already closed — the board caught up",
        ),
    ] {
        if count > 0 {
            eprintln!("{count} {what}");
        }
    }
}
