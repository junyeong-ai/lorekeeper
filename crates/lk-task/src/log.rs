use std::path::Path;

use lk_core::event::RawItem;
use serde::{Deserialize, Serialize};

use crate::TaskError;
use crate::task::{TaskId, TaskState};

/// What happened to a task.
///
/// The board holds STATE; this holds history, and the two answer different questions. "Carried
/// four days" and "planned nine, finished six" cannot be recovered from a file that only ever
/// shows what is still open, and a task that closed yesterday is gone from the board entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransitionKind {
    Created,
    Moved,
    /// A day closed with this task still committed to it.
    Carried,
    Done,
    Dropped,
}

impl TransitionKind {
    /// Whether this transition is an OBSERVATION of work — something that happened, which the
    /// ingest pipeline turns into a daily page like any other source's items.
    ///
    /// Only completion is. A dropped task is a decision worth keeping in the history, but the
    /// archive answers "what did I do", and deciding not to do a thing is not doing it.
    pub fn is_observation(self) -> bool {
        matches!(self, TransitionKind::Done)
    }

    /// Whether this transition ANSWERS the observation the task came from.
    ///
    /// Finishing and dropping both answer it; moving and carrying do not — the task is still
    /// open, and it is the BOARD that says so. Keeping the two questions apart is what lets a
    /// proposal need no store of its own.
    pub fn is_answer(self) -> bool {
        matches!(self, TransitionKind::Done | TransitionKind::Dropped)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub id: TaskId,
    pub at: jiff::Timestamp,
    pub kind: TransitionKind,
    /// The task's title as it read at the moment of the transition, so the history stays
    /// legible after the board has moved on and the task is no longer on it.
    pub title: String,
    /// The state a `Moved` transition arrived at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<TaskState>,
    /// What the closing command was told. This is the sentence the archive page carries, and
    /// through it the concept extraction — which is how work a person did becomes knowledge
    /// the vault holds, rather than only what they read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// How many day-closes the task had survived when it closed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub carried: u32,
    /// The day a `Carried` transition closed.
    ///
    /// Recorded rather than inferred from the day the transition was WRITTEN. "Has this task
    /// already been carried for this ended day" was asked of today's record, which is a proxy
    /// and not the fact: two closes on one actual day declaring two different ended days —
    /// catching up after a few days away — read as one, and the second was silently skipped.
    /// The task's own `carried_on` stamp answers exactly, and this is the same answer in the
    /// history, so the guard holds when a board write failed after the log write.
    ///
    /// A record written before this field existed carries none, and is not read as answering
    /// for any day — its absence is not the fact. What that leaves exposed is one command's
    /// window across the upgrade: a close whose log write landed and whose board write did not,
    /// with the next close on the other side of it. Where the board write DID land, the task's
    /// stamp answers and nothing is lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closing: Option<jiff::civil::Date>,
    /// The origin this task answered to, where it had one. See [`lk_core::origin`].
    ///
    /// Carried into the history so that "has this observation already been ANSWERED" is a
    /// question the two existing stores can settle between them — the board holds what is open,
    /// this holds what was finished or dropped — and a proposal needs no store of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Transition {
    pub fn new(
        id: TaskId,
        kind: TransitionKind,
        title: impl Into<String>,
        at: jiff::Timestamp,
    ) -> Self {
        Self {
            id,
            at,
            kind,
            title: title.into(),
            state: None,
            note: None,
            carried: 0,
            closing: None,
            src: None,
        }
    }

    pub fn with_closing(mut self, closing: jiff::civil::Date) -> Self {
        self.closing = Some(closing);
        self
    }

    pub fn with_src(mut self, src: Option<String>) -> Self {
        self.src = src;
        self
    }

    pub fn with_state(mut self, state: TaskState) -> Self {
        self.state = Some(state);
        self
    }

    pub fn with_note(mut self, note: Option<String>) -> Self {
        self.note = note.filter(|text| !text.trim().is_empty());
        self
    }

    pub fn with_carried(mut self, carried: u32) -> Self {
        self.carried = carried;
        self
    }

    /// The observation this transition makes, for the ingest pipeline to render.
    ///
    /// A completed task re-enters through the SAME door every external source uses, which is
    /// what makes the archive cost nothing to build: the daily page, the work-log, the
    /// contribution categories, the concept extraction and every review already consume events,
    /// so they consume this without changing.
    pub fn observation(&self) -> Option<RawItem> {
        if !self.kind.is_observation() {
            return None;
        }
        let mut body = self.note.clone().unwrap_or_default();
        if self.carried > 0 {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str(&format!("(carried {} day(s))", self.carried));
        }
        Some(RawItem {
            // The task, and nothing else. `EventId` already carries the date, so this is one
            // observation per task per day: a completion recorded twice — the log written, the
            // board write failing, the reconcile finding the box still ticked — collapses in the
            // pipeline's own dedup instead of appearing twice on the page, in the work-log and
            // in the performance record. Including the instant made every retry a new identity.
            external_id: Some(format!("task:{}", self.id)),
            title: self.title.clone(),
            body,
            url: None,
            author: None,
            timestamp: self.at,
            // A task on this board is the user's own by construction; there is no other author
            // it could have, so ownership is not inferred, it is structural.
            is_self: true,
            open_work: None,
            metadata: serde_json::json!({
                "task_id": self.id.as_str(),
                "carried": self.carried,
            }),
        })
    }
}

/// What the history already holds about the tasks a pass is about to act on.
///
/// Two questions with two different windows, which one list of transitions cannot answer at
/// once. "Has this completion already been recorded?" must look PAST midnight: a board write
/// that fails in the evening is repaired by the next morning's pass, and asking only that
/// morning's record found nothing, harvested the tick again, and archived one completion on two
/// dates — which two `EventId`s cannot collapse. "Was this task already carried FOR THIS ENDED
/// DAY?" is keyed on the PAIR, so it needs no window of its own and must be found wherever the
/// transition was written: a carry for another day cannot suppress this one, and asked over
/// today alone a close retried on a later calendar day wrote a second carry for one ended day.
#[derive(Debug, Default, Clone)]
pub struct Recorded {
    /// The completion the window holds for an id, and the DAY the record holds it under.
    ///
    /// The day is what makes the guard's rule per-TASK. It reads "a completion recorded on or
    /// after this task's own first day is this task's", and the window that finds one is bounded
    /// by the EARLIEST `since` among every ticked line — so one long-standing task ticked beside
    /// a newer one widened the window for both, and a completion two weeks older than the newer
    /// task's own first day settled it. Its real completion reached nothing at all.
    closed: std::collections::BTreeMap<TaskId, (jiff::civil::Date, Transition)>,
    carried: std::collections::BTreeSet<(TaskId, jiff::civil::Date)>,
    /// The last day the history records a task ENTERING `## Today`, whichever route took it
    /// there. What makes "was this committed to the day being closed" answerable from the
    /// record instead of from the shape the board happened to have when a pass began.
    committed: std::collections::BTreeMap<TaskId, jiff::civil::Date>,
    seen: std::collections::BTreeSet<TaskId>,
    answered: std::collections::BTreeSet<String>,
}

impl Recorded {
    /// One day's record answering both questions — what a single date's file supports.
    pub fn from_day(day: jiff::civil::Date, transitions: &[Transition]) -> Self {
        let mut recorded = Self::default();
        for transition in transitions {
            recorded.absorb(transition.clone(), day, true);
        }
        recorded
    }

    /// One transition, in the order the history holds it.
    ///
    /// A `Created` CLEARS whatever closure stands for that id rather than being compared to it.
    /// An id is minted against the ids on the board, so one freed by a completion can be minted
    /// again — and then the earlier task's `Done` sits in the window describing a task that no
    /// longer exists. Comparing instants at `Done` time does not settle it: the earlier `Done`
    /// is correctly credited when it is read, and nothing retracts that credit once the id's
    /// next life begins. Clearing does, in one forward pass — what survives to the end can only
    /// be a completion recorded since that id's own most recent creation. Where the window holds
    /// no `Created` for an id — a stamp typed by hand, a task older than the window — there is
    /// nothing to clear and the window's own start is what bounds it.
    fn absorb(
        &mut self,
        transition: Transition,
        day: jiff::civil::Date,
        in_completion_window: bool,
    ) {
        self.seen.insert(transition.id.clone());
        if let Some(src) = &transition.src
            && transition.kind.is_answer()
        {
            self.answered.insert(src.clone());
        }
        // The DAY the record holds it under, not the instant it names: every date in this plane
        // is derived in the vault's zone once, when the transition is filed, and re-deriving it
        // here from a zone this does not have is how the two answers come apart.
        if transition.state == Some(crate::task::TaskState::Today) {
            self.committed.insert(transition.id.clone(), day);
        }
        match transition.kind {
            TransitionKind::Created => {
                // An id minted again is a NEW task, so nothing its previous life recorded is
                // its. The closure is cleared inside the completion window, where reading
                // further back would let an older life settle a live line; the carries are
                // cleared wherever they were written, because they are keyed on the pair and
                // are absorbed the same way. Left standing, a carry from a previous life
                // suppressed a real one for that day and the count that diagnoses a stale task
                // undercounted with nothing saying so.
                self.carried.retain(|(id, _)| id != &transition.id);
                if in_completion_window {
                    self.closed.remove(&transition.id);
                }
            }
            TransitionKind::Done if in_completion_window => {
                self.closed.insert(transition.id.clone(), (day, transition));
            }
            // Absorbed wherever it was written, because the key is the PAIR: a carry for
            // another ended day cannot suppress this one, so there is nothing for a window to
            // protect against. Gated on the transition sitting in today's file, a close retried
            // on a later calendar day found nothing and wrote a second carry for one day.
            TransitionKind::Carried => {
                if let Some(closing) = transition.closing {
                    self.carried.insert((transition.id, closing));
                }
            }
            _ => {}
        }
    }

    /// Every origin the window records an ANSWER for — a task finished or dropped.
    ///
    /// What stops an observation being proposed every morning after the person has already
    /// dealt with it. A move or a carry is not an answer: the task is still open, and it is the
    /// BOARD that says so.
    pub fn answered(&self) -> &std::collections::BTreeSet<String> {
        &self.answered
    }

    /// Every id the window mentions at all.
    ///
    /// Minting against these as well as the board's is what makes that recycle impossible inside
    /// the window rather than merely survivable: two tasks sharing one id on one date are one
    /// entry in that date's record, which holds ONE completion per task — so the earlier task's
    /// completion is not duplicated, it is overwritten, and it never reaches the archive.
    pub fn seen(&self) -> impl Iterator<Item = &TaskId> {
        self.seen.iter()
    }

    /// The completion the history holds for `id`, if it holds one.
    ///
    /// The window is walked forward, so where an id was recycled this is the LATEST completion
    /// recorded under it — the one a line still ticked on the board can belong to.
    pub fn closure(&self, id: &TaskId) -> Option<&Transition> {
        self.closed.get(id).map(|(_, closed)| closed)
    }

    /// Whether the history already holds THIS task's completion.
    ///
    /// Asked with the task's own first day, because that is the whole reason a completion under
    /// its id can be trusted to be its: an id is minted against the ids on the board, so a
    /// recycled id's previous owner left before this task was written down. A closure recorded
    /// BEFORE that day belongs to that previous owner, and taking it made the ticked line
    /// vanish with its completion recorded nowhere.
    pub(crate) fn is_closed(&self, id: &TaskId, since: jiff::civil::Date) -> bool {
        self.closed.get(id).is_some_and(|(day, _)| *day >= since)
    }

    pub(crate) fn is_carried(&self, id: &TaskId, closing: jiff::civil::Date) -> bool {
        self.carried.contains(&(id.clone(), closing))
    }

    /// Whether the history records this task entering `## Today` after `day` ended.
    ///
    /// A one-directional proof, and deliberately so. It answers YES only on a record of a later
    /// commitment, so it can refuse a carry the day never held and can never invent one. Silence
    /// is not evidence: a line dragged under the heading in an editor is a state change nothing
    /// records, and reading its absence as "never committed" would drop the carry of every task
    /// managed the way the board is meant to be.
    pub(crate) fn entered_today_after(&self, id: &TaskId, day: jiff::civil::Date) -> bool {
        self.committed.get(id).is_some_and(|entered| *entered > day)
    }
}

/// The origins the history has an answer for, and which answer.
///
/// One question suppresses and a different one reports, and folding them together made the
/// report tell a person to undo their own decisions. A DROP is a decision that stands: naming it
/// every morning asks them to write down again what they deliberately said no to, and the count
/// grows with every correct use of `lore task drop`. A COMPLETION is a statement about the work
/// at a time, so a source still declaring it open is the one thing here worth mentioning.
#[derive(Debug, Default)]
pub struct Answered {
    finished: std::collections::BTreeSet<String>,
    declined: std::collections::BTreeSet<String>,
}

impl Answered {
    /// Record what `kind` says about `origin`. A move or a carry says nothing — the task is
    /// still open, and it is the BOARD that says so.
    /// The LAST answer stands, which is what "not since declined" means. Kept as two sets a
    /// transition only ever added to, an origin dropped in March and done in August answered
    /// both ways at once and the report could not tell which the person meant last.
    pub(crate) fn absorb(&mut self, origin: String, kind: TransitionKind) {
        match kind {
            TransitionKind::Done => {
                self.declined.remove(&origin);
                self.finished.insert(origin);
            }
            TransitionKind::Dropped => {
                self.finished.remove(&origin);
                self.declined.insert(origin);
            }
            _ => {}
        }
    }

    /// Whether the history has ANY answer for `origin`. What stops it being proposed again.
    pub fn contains(&self, origin: &str) -> bool {
        self.finished.contains(origin) || self.declined.contains(origin)
    }

    /// Whether `origin` was finished and never since declined — the only answer a person might
    /// want back, and so the only one worth naming.
    pub fn is_recoverable(&self, origin: &str) -> bool {
        self.finished.contains(origin)
    }
}

/// The per-date transition history.
///
/// One file per date, in the same shape and for the same reason as the streaming sources' event
/// log: a day's record is durable and complete on its own, so `lore ingest --date <past>`
/// reproduces that day's archive exactly rather than approximately.
pub struct TransitionLog {
    shelf: crate::store::Shelf,
}

impl TransitionLog {
    pub fn new(vault_root: &Path) -> Self {
        Self {
            shelf: crate::store::Shelf::at(vault_root.join(".lorekeeper").join("tasks")),
        }
    }

    /// Every date the record holds, oldest first.
    ///
    /// A key that is not a date is an ERROR rather than a file to skip. This directory is the
    /// transition log's alone, so a name that is not a date is damage — a sync client's conflict
    /// copy holds real completions, and passing over it silently would re-propose exactly the
    /// work it says was answered and let a completion be recorded twice. `Jsonl::read` refuses a
    /// line it cannot read for the same reason; the strictness must not go quiet at the filename.
    pub fn dates(&self) -> Result<Vec<jiff::civil::Date>, TaskError> {
        self.shelf
            .keys()?
            .into_iter()
            .map(|key| {
                key.parse().map_err(|e| {
                    TaskError::Malformed(format!(
                        "the task record holds `{key}.jsonl`, which is not a date: {e} — \
                         recover what it holds into the day it belongs to, or delete it. Left \
                         intact and refused rather than passed over, because a conflict copy \
                         holds completions and reading around it records them twice"
                    ))
                })
            })
            .collect()
    }

    /// A date's transitions, oldest first, or nothing where the day has none.
    ///
    /// An unreadable line is a hard error rather than a skip: the writer only ever produces
    /// this file through an atomic replace, so a malformed line is external corruption — and
    /// silently dropping it would let the next append rewrite the file without it, turning
    /// damage into permanent loss.
    pub fn read(&self, date: jiff::civil::Date) -> Result<Vec<Transition>, TaskError> {
        self.shelf.file(&date.to_string()).read()
    }

    /// What the history holds about the tasks on `board`.
    ///
    /// The completion window starts at the earliest `since` among the board's TICKED tasks and
    /// nothing wider. It is bounded by data rather than by a guess: an id is minted against the
    /// ids currently on the board, so a recycled id's previous owner left the board before this
    /// task was written down, and a completion recorded on or after this task's own first day is
    /// this task's. A board holding no ticked line asks about today alone, which is every
    /// ordinary pass.
    /// `floor` widens the window for a question the board cannot bound. A day-close asks "have
    /// I already carried this task for the day I am closing", and the transition answering that
    /// was written on whatever day the close first ran — at or after the closing day, and not
    /// necessarily today. Asked over today alone, a close retried on a LATER calendar day found
    /// nothing and wrote a second carry for one ended day.
    pub fn recorded_for(
        &self,
        board: &crate::Board,
        today: jiff::civil::Date,
        floor: Option<jiff::civil::Date>,
    ) -> Result<Recorded, TaskError> {
        // Two starts, because the two questions are bounded by different things. A completion
        // is bounded by the ticked task's own first day — that is what makes "a completion
        // recorded on or after this task's own first day is this task's" true, and reading
        // further back for it would let an older life of a recycled id settle a live line. A
        // carry is bounded by the day being closed. Widening one window for both made `sync`
        // and `rollover` disagree about the same tick.
        let completions_from = board
            .tasks()
            .filter(|task| task.done)
            .map(|task| task.since)
            .min()
            .unwrap_or(today)
            .min(today);
        let from = floor.map_or(completions_from, |floor| completions_from.min(floor));

        // The dates the record actually HOLDS, rather than every date from there to today. Two
        // things follow from asking the shelf instead of counting days. There is no CEILING: a
        // completion filed on a date ahead of this pass's today — a `vault.timezone` moved
        // westward, two machines whose clocks differ — sat outside the window, so the tick was
        // harvested a second time and one completion was archived on two dates, which two
        // `EventId`s cannot collapse. And a floor far in the past costs one directory read
        // rather than an `open` for every day in between.
        let mut recorded = Recorded::default();
        for day in self.dates()? {
            if day < from {
                continue;
            }
            for transition in self.read(day)? {
                recorded.absorb(transition, day, day >= completions_from);
            }
        }
        Ok(recorded)
    }

    /// What the WHOLE history records an answer for.
    ///
    /// Read across every date rather than over a window, because the question is not "what
    /// happened lately" but "have I dealt with this before" — an issue dropped in March must
    /// not be proposed again in August. The cost is one directory of small files read once by
    /// a command that runs at most daily, and it is exact: `lore maintenance` prunes the ingest
    /// log and drained queue files, never this, so no horizon can turn an answered origin back
    /// into an unanswered one.
    ///
    /// A date whose record will not read is a hard error here, as everywhere: treating it as
    /// empty would re-propose exactly the work the unreadable half says was finished.
    pub fn answered_origins(&self) -> Result<Answered, TaskError> {
        let mut answered = Answered::default();
        for date in self.dates()? {
            for transition in self.read(date)? {
                if let Some(src) = transition.src {
                    answered.absorb(src, transition.kind);
                }
            }
        }
        Ok(answered)
    }

    /// Add `transitions` to their dates' records.
    ///
    /// Read-modify-write through the workspace's one atomic writer rather than an append: a
    /// day holds tens of lines, so rewriting costs nothing, and it is the same durability every
    /// other file this tool produces gets. One command mutates the board at a time, so there is
    /// no in-run race to lose an entry to.
    ///
    /// A day holds ONE completion per task, so an observation REPLACES the one already there
    /// rather than joining it. That is not a new rule — `Transition::observation` keys its item
    /// on `task:{id}` and `EventId` already carries the date, so a day's second completion of
    /// one task collapses in the pipeline's dedup and only the record would have shown two.
    /// Stating it here is what lets a note reach a completion already written: the amendment is
    /// that transition plus the sentence, so the instant, the carry count and the title it was
    /// closed under are the ones the day recorded. Every other kind is appended — a task moved
    /// out of today and back is two moves, and reading them as one would lose the day's shape.
    pub fn record(
        &self,
        transitions: &[Transition],
        zone: &jiff::tz::TimeZone,
    ) -> Result<(), TaskError> {
        let mut by_date: std::collections::BTreeMap<jiff::civil::Date, Vec<&Transition>> =
            std::collections::BTreeMap::new();
        for transition in transitions {
            by_date
                .entry(transition.at.to_zoned(zone.clone()).date())
                .or_default()
                .push(transition);
        }

        for (date, added) in by_date {
            let file = self.shelf.file(&date.to_string());
            let mut day: Vec<Transition> = file.read()?;
            for transition in added {
                match day.iter_mut().find(|held| {
                    held.id == transition.id
                        && held.kind.is_observation()
                        && transition.kind.is_observation()
                }) {
                    Some(held) => *held = transition.clone(),
                    None => day.push(transition.clone()),
                }
            }
            file.replace(&day)?;
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

    fn id(text: &str) -> TaskId {
        text.parse().unwrap()
    }

    fn done(note: Option<&str>) -> Transition {
        Transition::new(id("7k2p"), TransitionKind::Done, "read the spec", at(9))
            .with_note(note.map(str::to_string))
    }

    #[test]
    fn a_day_round_trips_through_its_record() {
        let tmp = tempfile::tempdir().unwrap();
        let log = TransitionLog::new(tmp.path());
        let zone = jiff::tz::TimeZone::UTC;

        log.record(&[done(Some("a finding"))], &zone).unwrap();
        log.record(
            &[Transition::new(
                id("3b8q"),
                TransitionKind::Carried,
                "other",
                at(18),
            )],
            &zone,
        )
        .unwrap();

        let day = log.read(jiff::civil::date(2026, 8, 19)).unwrap();
        assert_eq!(day.len(), 2, "recording twice adds, never replaces");
        assert_eq!(day[0].note.as_deref(), Some("a finding"));
        assert_eq!(day[1].kind, TransitionKind::Carried);
    }

    #[test]
    fn a_day_with_no_record_is_empty_rather_than_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let log = TransitionLog::new(tmp.path());
        assert!(log.read(jiff::civil::date(2026, 8, 19)).unwrap().is_empty());
    }

    /// Damage is surfaced, never amplified: skipping the line would let the next record rewrite
    /// the file without it.
    #[test]
    fn a_corrupt_line_is_an_error_rather_than_a_line_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let log = TransitionLog::new(tmp.path());
        let date = jiff::civil::date(2026, 8, 19);
        log.record(&[done(None)], &jiff::tz::TimeZone::UTC).unwrap();

        let path = tmp.path().join(".lorekeeper/tasks/2026-08-19.jsonl");
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("{not json}\n");
        std::fs::write(&path, content).unwrap();

        assert!(matches!(log.read(date), Err(TaskError::Malformed(_))));
    }

    /// The transition's own instant decides which day's record it lands in, read through the
    /// vault's timezone like every other date in this tool — never UTC.
    #[test]
    fn a_transition_lands_on_its_own_day_in_the_vaults_zone() {
        let tmp = tempfile::tempdir().unwrap();
        let log = TransitionLog::new(tmp.path());
        let seoul = jiff::tz::TimeZone::get("Asia/Seoul").unwrap();

        // 22:00 UTC on the 19th is 07:00 on the 20th in Seoul.
        log.record(&[done(None)], &seoul).unwrap();
        log.record(
            &[Transition::new(
                id("3b8q"),
                TransitionKind::Done,
                "late",
                at(22),
            )],
            &seoul,
        )
        .unwrap();

        assert_eq!(log.read(jiff::civil::date(2026, 8, 19)).unwrap().len(), 1);
        assert_eq!(log.read(jiff::civil::date(2026, 8, 20)).unwrap().len(), 1);
    }

    #[test]
    fn only_a_completion_is_an_observation() {
        assert!(done(None).observation().is_some());
        for kind in [
            TransitionKind::Created,
            TransitionKind::Moved,
            TransitionKind::Carried,
            TransitionKind::Dropped,
        ] {
            let transition = Transition::new(id("7k2p"), kind, "t", at(9));
            assert!(transition.observation().is_none(), "{kind:?}");
        }
    }

    #[test]
    fn a_completions_note_is_the_body_the_archive_carries() {
        let item = done(Some("refresh tokens rotate on use"))
            .observation()
            .unwrap();
        assert_eq!(item.body, "refresh tokens rotate on use");
        assert!(item.is_self, "a task on this board has no other author");
        assert_eq!(item.title, "read the spec");
    }

    /// One observation per task per day. A completion recorded twice — the log written and the
    /// board write failing, so the next reconcile finds the box still ticked — must collapse
    /// rather than appear twice on the page, in the work-log and in the performance record. The
    /// DAY is carried by `EventId`, so two completions on different days stay two.
    #[test]
    fn a_completion_recorded_twice_is_one_observation() {
        let first = done(None).observation().unwrap();
        let retried = Transition::new(id("7k2p"), TransitionKind::Done, "read the spec", at(17));
        assert_eq!(
            first.external_id,
            retried.observation().unwrap().external_id
        );

        let other = Transition::new(id("3b8q"), TransitionKind::Done, "other", at(9));
        assert_ne!(first.external_id, other.observation().unwrap().external_id);
    }

    /// A note supplied after the day already recorded the completion joins that completion,
    /// keeping the instant and the carry count the day wrote down. Appending instead would put
    /// two of one finished task into the record, where only the pipeline's dedup would have
    /// hidden it.
    #[test]
    fn a_note_joins_the_completion_the_day_already_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = TransitionLog::new(tmp.path());
        let zone = jiff::tz::TimeZone::UTC;

        log.record(&[done(None).with_carried(2)], &zone).unwrap();
        let amended = done(None)
            .with_carried(2)
            .with_note(Some("the rounding was in the tax line".into()));
        log.record(&[amended], &zone).unwrap();

        let day = log.read(jiff::civil::date(2026, 8, 19)).unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(
            day[0].note.as_deref(),
            Some("the rounding was in the tax line")
        );
        assert_eq!(day[0].at, at(9));
        assert_eq!(day[0].carried, 2);
    }

    /// Only a completion is keyed that way. A task moved out of today and back is two moves,
    /// and reading them as one would lose the shape of the day.
    #[test]
    fn two_moves_of_one_task_stay_two() {
        let tmp = tempfile::tempdir().unwrap();
        let log = TransitionLog::new(tmp.path());
        let zone = jiff::tz::TimeZone::UTC;

        log.record(
            &[
                Transition::new(id("7k2p"), TransitionKind::Moved, "read the spec", at(9))
                    .with_state(TaskState::Next),
                Transition::new(id("7k2p"), TransitionKind::Moved, "read the spec", at(14))
                    .with_state(TaskState::Today),
            ],
            &zone,
        )
        .unwrap();

        assert_eq!(log.read(jiff::civil::date(2026, 8, 19)).unwrap().len(), 2);
    }

    /// An id freed by a completion can be minted again, and then the earlier task's `Done`
    /// stands in the window describing a task that no longer exists — so a line still ticked for
    /// the NEW task reads as already closed and its completion is never recorded. The id's own
    /// creation is what ends the earlier life, so it clears rather than being compared against.
    #[test]
    fn a_completion_does_not_outlive_the_task_that_earned_it() {
        let recycled = id("7k2p");
        let history = [
            Transition::new(recycled.clone(), TransitionKind::Created, "first", at(7)),
            Transition::new(recycled.clone(), TransitionKind::Done, "first", at(9)),
            Transition::new(recycled.clone(), TransitionKind::Created, "second", at(10)),
        ];
        let recorded = Recorded::from_day(jiff::civil::date(2026, 8, 19), &history);
        assert!(recorded.closure(&recycled).is_none());

        // The task's own completion still answers, and its title is the one it closed under.
        let mut whole = history.to_vec();
        whole.push(Transition::new(
            recycled.clone(),
            TransitionKind::Done,
            "second",
            at(17),
        ));
        assert_eq!(
            Recorded::from_day(jiff::civil::date(2026, 8, 19), &whole)
                .closure(&recycled)
                .unwrap()
                .title,
            "second"
        );
    }

    /// A window holding no `Created` for an id — a stamp typed by hand, a task older than the
    /// window — has nothing to clear, and the window's own start is what bounds it.
    #[test]
    fn a_completion_with_no_creation_in_the_window_still_answers() {
        let recorded = Recorded::from_day(jiff::civil::date(2026, 8, 19), &[done(None)]);
        assert!(recorded.closure(&id("7k2p")).is_some());
    }

    #[test]
    fn every_id_the_window_mentions_is_offered_to_the_mint() {
        let recorded = Recorded::from_day(
            jiff::civil::date(2026, 8, 19),
            &[
                Transition::new(id("7k2p"), TransitionKind::Created, "a", at(7)),
                Transition::new(id("3b8q"), TransitionKind::Dropped, "b", at(9)),
            ],
        );
        let seen: Vec<_> = recorded.seen().cloned().collect();
        assert_eq!(seen, [id("3b8q"), id("7k2p")]);
    }

    #[test]
    fn a_carry_count_reaches_the_archive() {
        let item = done(None).with_carried(4).observation().unwrap();
        assert!(item.body.contains("carried 4"), "{}", item.body);
    }
}
