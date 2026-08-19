use std::collections::BTreeSet;
use std::fmt::Write as _;

use lk_core::i18n::Locale;

use crate::TaskError;
use crate::task::{Task, TaskId, TaskState};

/// One line under a section heading.
///
/// Three kinds because a board is edited by two writers with different rights. A stamped line
/// is this tool's; a checkbox line without a stamp is a task someone typed in an editor and is
/// adopted on the next reconcile; anything else — a note, a blank line, a nested list — belongs
/// to whoever wrote it and is carried through untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Task(Task),
    /// A checkbox line with no machine stamp: a task added by hand.
    Unstamped {
        title: String,
        done: bool,
    },
    Verbatim(String),
}

/// The open tasks, as the file holds them.
///
/// The FILE is the truth rather than a rendering of some store elsewhere, because the vault is
/// the product: a checkbox ticked on a phone has to count, and a design that keeps the state
/// somewhere else discards that edit silently — which is worse than any parsing risk. What
/// makes the parsing risk small is that a line this cannot read is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    sections: Vec<(TaskState, Vec<Entry>)>,
    /// Content between the generated header and the first section heading.
    prologue: Vec<String>,
    /// Lines carrying a stamp that would not parse, by 1-based line number. Reported rather
    /// than repaired: the bytes are somebody's, and a guess at what a corrupted date meant is
    /// a task silently rescheduled.
    malformed: Vec<(usize, String)>,
    /// The line a code fence opened on that the page never closed.
    unterminated_fence: Option<usize>,
    /// Lines carrying an id an earlier line already claims.
    duplicated: Vec<(TaskId, usize)>,
}

impl Default for Board {
    fn default() -> Self {
        Self::empty()
    }
}

impl Board {
    pub fn empty() -> Self {
        Self {
            sections: TaskState::ALL.map(|state| (state, Vec::new())).to_vec(),
            prologue: Vec::new(),
            malformed: Vec::new(),
            unterminated_fence: None,
            duplicated: Vec::new(),
        }
    }

    /// Read a board from the page's text.
    ///
    /// Headings are matched in EVERY locale, not the configured one: a `vault.locale` switch
    /// renames all four at once, and a reader that searched only the new spelling would find no
    /// sections, treat every task line as prologue, and write them back under empty headings.
    pub fn parse(page: &str) -> Self {
        let body = strip_frontmatter(page);
        // Every line this reports is a line the user will go and look at, so it is numbered in
        // the FILE rather than in the body the parse walks. Reporting the body's number sent
        // them past the frontmatter to whatever happened to sit there.
        let offset = page.lines().count() - body.lines().count();
        let mut board = Board::empty();
        let mut current: Option<TaskState> = None;
        let mut claimed: std::collections::BTreeSet<TaskId> = std::collections::BTreeSet::new();
        let mut fence: Option<Fence> = None;
        let mut title_seen = false;

        for (index, line) in body.lines().enumerate() {
            let trimmed = line.trim_end();

            // Inside a fenced code block nothing is markup. A board documenting its own format
            // — which the generated page format reference does — holds both a `## Today` and a
            // checkbox line as EXAMPLES, and reading them as structure both deletes the heading
            // and adopts the example as a real task on the next reconcile.
            match &fence {
                Some(open) => {
                    if open.closed_by(trimmed) {
                        fence = None;
                        board.unterminated_fence = None;
                    }
                    // Verbatim without exception: a blank line inside a code block is part of
                    // the code, and two in a row are two, so the rules that decide which blanks
                    // are layout do not apply in here.
                    board.keep_exactly(current, trimmed);
                    continue;
                }
                None => {
                    if let Some(open) = Fence::opened_by(trimmed) {
                        fence = Some(open);
                        board.unterminated_fence = Some(index + 1 + offset);
                        board.keep_exactly(current, trimmed);
                        continue;
                    }
                }
            }

            if let Some(state) = heading_state(trimmed) {
                current = Some(state);
                continue;
            }
            // The generated title is the FIRST `# ` line and only that one. Guarding on an
            // empty prologue instead still ate the second heading whenever only blank lines
            // separated the two — which is exactly how a person writes their own heading, right
            // under the title this tool wrote.
            if current.is_none() && !title_seen && trimmed.starts_with("# ") {
                title_seen = true;
                continue;
            }
            let Some(state) = current else {
                board.keep(None, trimmed);
                continue;
            };
            match read_line(trimmed) {
                Ok(Some(entry)) => {
                    if let Entry::Task(task) = &entry
                        && !claimed.insert(task.id.clone())
                    {
                        board.duplicated.push((task.id.clone(), index + 1 + offset));
                    }
                    board.entry_mut(state).push(entry);
                }
                Ok(None) => board.keep(Some(state), trimmed),
                Err(_) => {
                    board
                        .malformed
                        .push((index + 1 + offset, trimmed.to_string()));
                    board.keep(Some(state), trimmed);
                }
            }
        }
        board.retag();
        board
    }

    /// A parsed task's `state` comes from the heading it was found under, so it is assigned
    /// after the walk rather than read off the line — moving a line to another heading in an
    /// editor IS the state change, and a stamp that disagreed would undo it.
    fn retag(&mut self) {
        for (state, entries) in &mut self.sections {
            for entry in entries.iter_mut() {
                if let Entry::Task(task) = entry {
                    task.state = *state;
                }
            }
            // A blank line divides prose only when prose follows it. One at the end of a
            // section divides nothing — it is the gap the render puts before the next heading,
            // and keeping it would add another gap every time the page was written back.
            while matches!(entries.last(), Some(Entry::Verbatim(line)) if line.is_empty()) {
                entries.pop();
            }
        }
        while self.prologue.last().is_some_and(|line| line.is_empty()) {
            self.prologue.pop();
        }
    }

    /// Keep a line exactly as it reads, under whichever section it was found in — or in the
    /// prologue, where it was found before any.
    fn keep(&mut self, current: Option<TaskState>, line: &str) {
        let Some(state) = current else {
            return self.keep_in_prologue(line);
        };
        let held = self.entry_mut(state);
        if line.is_empty() {
            // A blank line after anything at all separates what a person wrote from what
            // follows it — after a TASK line too, where dropping it made the note below read as
            // a lazy continuation of that task's list item in every CommonMark renderer. Never
            // doubled, and trailing ones are trimmed at the end of the walk, which is what
            // keeps the page a fixed point of parse-then-render.
            let divides = match held.last() {
                Some(Entry::Verbatim(previous)) => !previous.is_empty(),
                Some(_) => true,
                None => false,
            };
            if divides {
                held.push(Entry::Verbatim(String::new()));
            }
            return;
        }
        held.push(Entry::Verbatim(line.to_string()));
    }

    fn keep_exactly(&mut self, current: Option<TaskState>, line: &str) {
        match current {
            Some(state) => self
                .entry_mut(state)
                .push(Entry::Verbatim(line.to_string())),
            None => self.prologue.push(line.to_string()),
        }
    }

    fn keep_in_prologue(&mut self, line: &str) {
        if line.is_empty() {
            if self.prologue.last().is_some_and(|last| !last.is_empty()) {
                self.prologue.push(String::new());
            }
            return;
        }
        self.prologue.push(line.to_string());
    }

    fn entry_mut(&mut self, state: TaskState) -> &mut Vec<Entry> {
        &mut self
            .sections
            .iter_mut()
            .find(|(s, _)| *s == state)
            .expect("every state has a section")
            .1
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.sections
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .filter_map(|entry| match entry {
                Entry::Task(task) => Some(task),
                _ => None,
            })
    }

    pub fn tasks_mut(&mut self) -> impl Iterator<Item = &mut Task> {
        self.sections
            .iter_mut()
            .flat_map(|(_, entries)| entries.iter_mut())
            .filter_map(|entry| match entry {
                Entry::Task(task) => Some(task),
                _ => None,
            })
    }

    pub fn get(&self, id: &TaskId) -> Option<&Task> {
        self.tasks().find(|task| &task.id == id)
    }

    /// Every address the board already claims — the open tasks, and the ones sitting on lines
    /// this could not read.
    ///
    /// A quarantined line still names a task. Minting over its address would mean that
    /// repairing that line later produced two live tasks answering to one id, and no command
    /// could then say which it meant.
    pub fn ids(&self) -> BTreeSet<TaskId> {
        self.tasks()
            .map(|task| task.id.clone())
            .chain(self.malformed.iter().filter_map(|(_, line)| {
                // Past the comment opener first: the line is stored whole, so the field reads
                // `<!--t:7k2p` and a bare `t:` prefix match never fired — which is every line
                // this tool writes, so the guard protected nothing.
                let stamp = line.rsplit_once("<!--")?.1;
                stamp
                    .trim_end_matches("-->")
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("t:"))
                    .and_then(|value| value.parse().ok())
            }))
            .collect()
    }

    pub fn malformed(&self) -> &[(usize, String)] {
        &self.malformed
    }

    /// Lines carrying an id an earlier line already claims, with the line each sits on.
    ///
    /// An id addresses a task, and every rule here reaches a task THROUGH one — so two lines
    /// answering to one id make every one of them ambiguous, and the ambiguity is resolved by
    /// file order, which is not an answer. `Board::remove` takes the FIRST line carrying the id:
    /// a copy of a stamped line placed above its ticked original had the wrong line deleted and
    /// the user's second task vanished with no transition anywhere, while a copy placed below it
    /// inherited the id, read as already closed on the pass that finished it, and its completion
    /// reached neither the history nor the archive.
    ///
    /// Duplicating a stamped line is how a person starts a similar task, so this is reachable
    /// without a collision in the mint. It is REPORTED rather than repaired: whether two lines
    /// with one address are two tasks or one task duplicated by a sync conflict is a judgment
    /// about what the person meant, and re-minting the copy would silently double a board that a
    /// merge duplicated. The same standing `lore graph lint` has on two pages answering to one
    /// name — and the message names the line and the one-character repair.
    ///
    /// What makes this complete is that it is asked on the SAME arm the live-task set is built
    /// from. A line reaches `tasks()`, `tasks_mut()` and `remove` only through `read_line`'s
    /// `Ok(Some(Entry::Task(_)))`, and that is where the id is claimed — so a checkbox inside a
    /// fence, one above the first heading, and one whose stamp will not read are all inert to
    /// `remove`, and none of them can be the second half of a collision. Moving this check off
    /// that arm is what would make a claimant invisible here while still visible to `remove`.
    ///
    /// It is deliberately NARROWER than [`Board::ids`], which also mines an id out of a line
    /// whose stamp will not read. Minting has to be conservative — an id it cannot see is one it
    /// can hand out twice — while this fires only where two LIVE tasks answer to one address,
    /// and reporting an inert line would teach a reader to distrust the message.
    pub fn duplicated(&self) -> &[(TaskId, usize)] {
        &self.duplicated
    }

    /// The line a code fence opened on that the page never closed, if there is one.
    ///
    /// CommonMark runs an unclosed fence to the end of the document, so the parse is right and
    /// the page is what is wrong: every heading below that line is code, and the render then
    /// emits a fresh set of sections after them — which the next parse swallows in turn. Left
    /// to run, `lore task rollover` grew the board by six lines a night, unattended. Where the
    /// fence opens before the first heading it is worse: no section is ever open, so every task
    /// is invisible to the tool while sitting in plain sight in the editor.
    ///
    /// So a READ still answers with what it could parse and a WRITE refuses. A wrong marker on
    /// the closing line (` ``` ` closed with `~~~`) is the ordinary way in, and it is one
    /// character to repair once the line is named.
    pub fn unterminated_fence(&self) -> Option<usize> {
        self.unterminated_fence
    }

    /// Every checkbox line typed without a stamp, in board order. The reconciler adopts them
    /// in place through [`adopt_unstamped`](Self::adopt_unstamped); this is how a test asks
    /// what a parse found before anything acted on it.
    #[cfg(test)]
    pub(crate) fn unstamped(&self) -> Vec<(TaskState, String, bool)> {
        self.sections
            .iter()
            .flat_map(|(state, entries)| {
                entries.iter().filter_map(move |entry| match entry {
                    Entry::Unstamped { title, done } => Some((*state, title.clone(), *done)),
                    _ => None,
                })
            })
            .collect()
    }

    /// Give every hand-typed checkbox an address, IN PLACE.
    ///
    /// In place rather than removed and re-added, because file order is the only priority this
    /// board has: a line typed at the top of Today is the thing being done now, and re-adding
    /// it would drop it to the bottom on the next pass — a daily annoyance that would also make
    /// "the file is the truth" false about the one thing the file says beyond its contents.
    pub(crate) fn adopt_unstamped(
        &mut self,
        mut mint: impl FnMut(TaskState, &str, bool) -> Task,
    ) -> Vec<Task> {
        let mut adopted = Vec::new();
        for (state, entries) in &mut self.sections {
            for entry in entries.iter_mut() {
                if let Entry::Unstamped { title, done } = entry {
                    let task = mint(*state, title, *done);
                    adopted.push(task.clone());
                    *entry = Entry::Task(task);
                }
            }
        }
        adopted
    }

    pub fn insert(&mut self, task: Task) {
        let state = task.state;
        self.entry_mut(state).push(Entry::Task(task));
    }

    /// Take a task off the board, or `None` where no task answers to `id`.
    pub fn remove(&mut self, id: &TaskId) -> Option<Task> {
        for (_, entries) in &mut self.sections {
            if let Some(at) = entries
                .iter()
                .position(|entry| matches!(entry, Entry::Task(task) if &task.id == id))
            {
                let Entry::Task(task) = entries.remove(at) else {
                    unreachable!("the position was found on a task")
                };
                return Some(task);
            }
        }
        None
    }

    /// Move a task to another section, keeping everything else about it.
    pub fn move_to(&mut self, id: &TaskId, state: TaskState) -> Result<Task, TaskError> {
        let mut task = self
            .remove(id)
            .ok_or_else(|| TaskError::Absent(id.clone()))?;
        task.state = state;
        let moved = task.clone();
        self.insert(task);
        Ok(moved)
    }

    /// The page this board writes back to, in the vault's current locale.
    pub fn render(&self, locale: Locale, today: jiff::civil::Date) -> String {
        let strings = locale.strings();
        let mut out = String::new();
        let _ = write!(
            out,
            "---\nid: {}\ntype: {}\ntitle: {:?}\nupdated: {today}\n---\n\n# {}\n",
            crate::BOARD_ID,
            crate::BOARD_FORMAT,
            strings.tasks_title,
            strings.tasks_title,
        );
        if !self.prologue.is_empty() {
            out.push('\n');
            for line in &self.prologue {
                let _ = writeln!(out, "{line}");
            }
        }
        for (state, entries) in &self.sections {
            let _ = write!(out, "\n## {}\n", state.heading(locale));
            if entries.is_empty() {
                continue;
            }
            out.push('\n');
            for entry in entries {
                match entry {
                    Entry::Task(task) => {
                        let _ = writeln!(out, "{}", render_task(task));
                    }
                    Entry::Unstamped { title, done } => {
                        let _ = writeln!(out, "- [{}] {title}", if *done { 'x' } else { ' ' });
                    }
                    Entry::Verbatim(line) => {
                        let _ = writeln!(out, "{line}");
                    }
                }
            }
        }
        out
    }
}

/// The line a task is written as: what a person reads, then an HTML comment.
///
/// A comment rather than a block id or a plugin's emoji grammar, because it is the one form
/// that is invisible in Obsidian's preview AND on GitHub, legal CommonMark, and survives a
/// round trip through both — the same test the vault's links are held to.
fn render_task(task: &Task) -> String {
    let mut stamp = format!("t:{} since:{}", task.id, task.since);
    if let Some(due) = task.due {
        let _ = write!(stamp, " due:{due}");
    }
    if let Some(wake) = task.wake {
        let _ = write!(stamp, " wake:{wake}");
    }
    if task.carried > 0 {
        let _ = write!(stamp, " carried:{}", task.carried);
    }
    if let Some(day) = task.carried_on {
        let _ = write!(stamp, " carried-on:{day}");
    }
    if let Some(src) = &task.src {
        let _ = write!(stamp, " src:{src}");
    }
    for (key, value) in &task.extra {
        let _ = write!(stamp, " {key}:{value}");
    }
    format!(
        "- [{}] {} <!--{stamp}-->",
        if task.done { 'x' } else { ' ' },
        task.title.trim()
    )
}

/// An open fenced code block, remembered by what opened it.
///
/// CommonMark closes a fence with the same character, at least as long, and nothing else on the
/// line — so a longer run inside a shorter fence is content rather than a close, and a fence
/// opened with backticks is not closed by tildes. Following that exactly is what keeps this
/// from being a guess about where code ends.
struct Fence {
    marker: char,
    length: usize,
}

impl Fence {
    fn opened_by(line: &str) -> Option<Self> {
        let start = line.trim_start();
        let marker = start.chars().next().filter(|c| *c == '`' || *c == '~')?;
        let length = start.chars().take_while(|c| *c == marker).count();
        (length >= 3).then_some(Fence { marker, length })
    }

    fn closed_by(&self, line: &str) -> bool {
        let start = line.trim_start();
        let length = start.chars().take_while(|c| *c == self.marker).count();
        length >= self.length && start[length..].trim().is_empty()
    }
}

/// A section heading, in whichever locale it was written under.
fn heading_state(line: &str) -> Option<TaskState> {
    let heading = line.strip_prefix("## ")?.trim();
    Locale::ALL
        .iter()
        .flat_map(|locale| TaskState::ALL.map(|state| (state, state.heading(*locale))))
        .find(|(_, spelling)| *spelling == heading)
        .map(|(state, _)| state)
}

/// One line under a heading: a task, a checkbox someone typed, or content that is not this
/// tool's. `Err` is a line carrying a stamp that would not read.
fn read_line(line: &str) -> Result<Option<Entry>, TaskError> {
    let Some((done, rest)) = checkbox(line) else {
        return Ok(None);
    };
    let Some((title, stamp)) = split_stamp(rest) else {
        // A line that CARRIES a stamp and does not end with it is not an unstamped line. Read
        // as one it was adopted under a fresh id, and the task it names left the board with no
        // transition of any kind — its carry count, its first day and the origin it answered to
        // gone, so a proposal treated this way is never answered and returns every morning. The
        // whole raw comment reached the archived title from there.
        //
        // A person typing a word after the stamp is how it happens, which is ordinary. So it is
        // refused like any stamp this cannot read: kept exactly, reported, and left out of
        // every rule until they move the text or delete the comment.
        if rest.contains("<!--") {
            return Err(TaskError::Malformed(
                "a stamp must be the last thing on its line — text after it would be read as a                  new task and the one this names would leave the board unrecorded"
                    .into(),
            ));
        }
        return Ok(Some(Entry::Unstamped {
            title: title_of(rest),
            done,
        }));
    };
    let mut task = read_stamp(stamp)?;
    task.title = title_of(title);
    task.done = done;
    Ok(Some(Entry::Task(task)))
}

/// A task line: a list item at column ZERO carrying a checkbox.
///
/// Every bullet CommonMark allows is accepted, because a line typed `* [ ] …` is a task the
/// person meant and silently keeping it as decoration is the worst of the three outcomes; the
/// render normalizes it, which it may, since the line is this tool's once it is a task.
///
/// Indentation is what it does NOT accept: an indented checkbox is a nested list item — a
/// sub-step of the line above it — and promoting it to a task of its own would take it out from
/// under the thing it belongs to.
fn checkbox(line: &str) -> Option<(bool, &str)> {
    let after_bullet = ['-', '*', '+']
        .iter()
        .find_map(|bullet| line.strip_prefix(&format!("{bullet} ")))?;
    for (mark, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
        if let Some(rest) = after_bullet.strip_prefix(mark) {
            return Some((done, rest));
        }
    }
    None
}

fn title_of(text: &str) -> String {
    text.trim().to_string()
}

/// Split a line's visible half from its machine half. The stamp is the LAST comment on the
/// line, so a title that itself contains one is left alone.
fn split_stamp(rest: &str) -> Option<(&str, &str)> {
    let trimmed = rest.trim_end();
    let close = trimmed.strip_suffix("-->")?;
    let open = close.rfind("<!--")?;
    Some((&close[..open], &close[open + 4..]))
}

/// The machine half: space-separated `key:value`, every value drawn from a strict alphabet so
/// nothing in it needs quoting and a value that would need it is a line this refuses to read.
///
/// A key this build does not know is KEPT rather than refused. The alphabet is what makes that
/// safe: an unknown field is still a well-formed pair, so carrying it costs nothing and the
/// board stays one file two builds can share.
fn read_stamp(stamp: &str) -> Result<Task, TaskError> {
    let mut id = None;
    let mut since = None;
    let mut due = None;
    let mut wake = None;
    let mut carried = 0u32;
    let mut carried_on = None;
    let mut src = None;
    let mut extra = Vec::new();

    for field in stamp.split_whitespace() {
        let (key, value) = field
            .split_once(':')
            .ok_or_else(|| TaskError::Malformed(format!("`{field}` is not a key:value")))?;
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.is_empty()
        {
            return Err(TaskError::Malformed(format!(
                "`{field}` has no readable value"
            )));
        }
        match key {
            "t" => id = Some(value.parse()?),
            "since" => since = Some(parse_date(value)?),
            "due" => due = Some(parse_date(value)?),
            "wake" => wake = Some(parse_date(value)?),
            "carried" => {
                carried = value
                    .parse()
                    .map_err(|_| TaskError::Malformed(format!("`{field}` is not a count")))?;
            }
            "carried-on" => carried_on = Some(parse_date(value)?),
            "src" => src = Some(value.to_string()),
            _ => extra.push((key.to_string(), value.to_string())),
        }
    }

    let id = id.ok_or_else(|| TaskError::Malformed("a stamp names no task id".into()))?;
    let since = since.ok_or_else(|| TaskError::Malformed("a stamp names no start date".into()))?;
    Ok(Task {
        id,
        title: String::new(),
        state: TaskState::Today,
        since,
        due,
        wake,
        carried,
        carried_on,
        src,
        done: false,
        extra,
    })
}

fn parse_date(value: &str) -> Result<jiff::civil::Date, TaskError> {
    value
        .parse()
        .map_err(|_| TaskError::Malformed(format!("`{value}` is not a date")))
}

/// The page past its frontmatter, whichever line ending wrote it.
///
/// A board edited on Windows arrives with `\r\n`; matching only `\n` treated the whole page as
/// body, so `id:`, `type:` and the two `---` fences came back as prologue lines below a freshly
/// generated frontmatter block, growing every time the page was written.
fn strip_frontmatter(page: &str) -> &str {
    let Some(rest) = page
        .strip_prefix("---\n")
        .or_else(|| page.strip_prefix("---\r\n"))
    else {
        return page;
    };
    match rest.split_once("\n---") {
        Some((_, body)) => body.trim_start_matches(['\n', '\r']),
        None => page,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i16, m: i8, d: i8) -> jiff::civil::Date {
        jiff::civil::date(y, m, d)
    }

    fn board_of(body: &str) -> Board {
        Board::parse(body)
    }

    #[test]
    fn a_stamped_line_reads_back_as_the_task_it_renders() {
        let mut task = Task::new(
            "7k2p".parse().unwrap(),
            "Atlassian refresh rotation",
            TaskState::Today,
            date(2026, 8, 17),
        );
        task.due = Some(date(2026, 8, 20));
        task.carried = 3;

        let mut board = Board::empty();
        board.insert(task.clone());
        let page = board.render(Locale::Ko, date(2026, 8, 19));

        let read = Board::parse(&page);
        assert_eq!(read.tasks().collect::<Vec<_>>(), vec![&task]);
        assert!(read.malformed().is_empty());
    }

    /// The stamp is invisible in both renderers this vault is read in, which is the whole
    /// reason it is a comment.
    #[test]
    fn the_visible_half_is_only_the_title() {
        let task = Task::new(
            "7k2p".parse().unwrap(),
            "read the spec",
            TaskState::Today,
            date(2026, 8, 19),
        );
        let line = render_task(&task);
        assert_eq!(line, "- [ ] read the spec <!--t:7k2p since:2026-08-19-->");
    }

    /// A `vault.locale` switch renames all four headings at once. A reader matching only the
    /// configured spelling would find no sections and write every task back into a file whose
    /// headings are empty.
    #[test]
    fn headings_are_read_in_every_locale_and_written_in_one() {
        let english = "## Today\n\n- [ ] a <!--t:7k2p since:2026-08-19-->\n\n## Waiting\n\n- [ ] b <!--t:3b8q since:2026-08-19-->\n";
        let board = board_of(english);
        let states: Vec<TaskState> = board.tasks().map(|task| task.state).collect();
        assert_eq!(states, [TaskState::Today, TaskState::Waiting]);

        let korean = board.render(Locale::Ko, date(2026, 8, 19));
        assert!(korean.contains("## 오늘"), "{korean}");
        assert!(korean.contains("## 대기"), "{korean}");
        assert_eq!(Board::parse(&korean).tasks().count(), 2);
    }

    /// Dragging a line under another heading is the state change, so the heading decides and a
    /// stamp cannot contradict it.
    #[test]
    fn the_heading_a_line_sits_under_decides_its_state() {
        let moved = "## Someday\n\n- [ ] a <!--t:7k2p since:2026-08-19-->\n";
        let board = board_of(moved);
        assert_eq!(board.tasks().next().unwrap().state, TaskState::Someday);
    }

    #[test]
    fn a_checkbox_without_a_stamp_is_a_task_someone_typed() {
        let board = board_of("## Today\n\n- [ ] wrote this in Obsidian\n");
        assert_eq!(
            board.unstamped(),
            [(
                TaskState::Today,
                "wrote this in Obsidian".to_string(),
                false
            )]
        );
        assert_eq!(board.tasks().count(), 0);
    }

    /// An id addresses a task, and every rule reaches a task through one — so two lines
    /// answering to one id make every rule ambiguous, and file order is not an answer to which
    /// line was meant. Duplicating a stamped line is how a person starts a similar task, so this
    /// needs no collision in the mint.
    #[test]
    fn a_line_claiming_an_id_an_earlier_line_holds_is_reported() {
        let page = concat!(
            "---\n",
            "id: tasks\n",
            "---\n",
            "\n",
            "# Tasks\n",
            "\n",
            "## Today\n",
            "\n",
            "- [x] write the migration <!--t:7k2p since:2026-08-18-->\n",
            "- [ ] write the migration (again) <!--t:7k2p since:2026-08-18-->\n",
        );
        let board = Board::parse(page);
        assert_eq!(board.duplicated().len(), 1);
        let (id, line) = &board.duplicated()[0];
        assert_eq!(id.as_str(), "7k2p");
        assert_eq!(*line, 10, "the line in the FILE, not in the body");

        // Both lines are still read and neither is rewritten: the page is reported on, never
        // repaired — which of the two the person meant is not knowable from the page.
        assert_eq!(board.tasks().count(), 2);
    }

    #[test]
    fn distinct_ids_are_not_reported() {
        let board = board_of("## Today\n\n- [ ] one <!--t:7k2p-->\n- [ ] two <!--t:3b8q-->\n");
        assert!(board.duplicated().is_empty());
    }

    /// Text typed after the stamp is not an unstamped line. Read as one, the line was adopted
    /// under a fresh id and the task it names left the board with NO transition — its carry
    /// count, its first day and the origin it answered to gone, and the raw comment carried
    /// into the archived title.
    #[test]
    fn a_stamp_that_is_not_last_on_its_line_is_refused_rather_than_adopted() {
        let board = board_of(
            "## Today\n\n- [x] done <!--t:7k2p since:2026-08-18--> and I typed this after\n",
        );
        assert_eq!(board.tasks().count(), 0, "not a task this can act on");
        assert_eq!(board.malformed().len(), 1);
        assert_eq!(
            board
                .render(Locale::En, date(2026, 8, 19))
                .matches("t:7k2p")
                .count(),
            1,
            "kept exactly once, not adopted beside itself"
        );
    }

    /// A corrupted stamp is somebody's bytes and a guess at what a broken date meant is a task
    /// silently rescheduled — so the line is kept exactly and named.
    #[test]
    fn a_line_whose_stamp_will_not_read_is_kept_and_reported() {
        let broken = "## Today\n\n- [ ] a <!--t:7k2p since:not-a-date-->\n";
        let board = board_of(broken);
        assert_eq!(board.tasks().count(), 0);
        assert_eq!(board.malformed().len(), 1);
        assert!(
            board
                .render(Locale::En, date(2026, 8, 19))
                .contains("since:not-a-date"),
            "the line survives the round trip it could not be read through"
        );
    }

    /// A board is one file two builds may open. Refusing a field the newer one added would take
    /// the whole line out of every rule on the older machine — not listed, not carried, not
    /// harvested — while looking perfectly normal in Obsidian the entire time.
    #[test]
    fn a_stamp_field_this_build_does_not_know_is_carried_through() {
        let board = board_of("## Today\n\n- [ ] a <!--t:7k2p since:2026-08-19 repeat:1w-->\n");
        assert!(board.malformed().is_empty());

        let task = board
            .tasks()
            .next()
            .expect("still a task this build can act on");
        assert_eq!(task.extra, [("repeat".to_string(), "1w".to_string())]);

        let rendered = board.render(Locale::En, date(2026, 8, 19));
        assert!(rendered.contains("repeat:1w"), "{rendered}");
        assert_eq!(
            Board::parse(&rendered).render(Locale::En, date(2026, 8, 19)),
            rendered,
            "and carrying it keeps the page a fixed point"
        );
    }

    /// What is still refused is a pair that is not a pair, or a value the alphabet does not
    /// admit — those are unreadable rather than merely unknown.
    #[test]
    fn a_stamp_that_is_not_well_formed_is_still_kept_and_reported() {
        for broken in [
            "t:7k2p since:2026-08-19 loose",
            "t:7k2p since:2026-08-19 k:a b",
        ] {
            let board = board_of(&format!("## Today\n\n- [ ] a <!--{broken}-->\n"));
            assert_eq!(board.malformed().len(), 1, "stamp `{broken}`");
        }
    }

    /// Anything that is not a checkbox belongs to whoever wrote it.
    #[test]
    fn content_that_is_not_a_task_is_carried_through() {
        let page = "## Today\n\n- [ ] a <!--t:7k2p since:2026-08-19-->\n> a note to self\n";
        let rendered = board_of(page).render(Locale::En, date(2026, 8, 19));
        assert!(rendered.contains("> a note to self"), "{rendered}");
    }

    /// A board that documents its own format holds a `## Today` and a checkbox line as
    /// EXAMPLES. Reading them as structure deleted the heading outright and left the example
    /// checkbox to be adopted as a real task by the next reconcile — a page teaching the format
    /// silently growing tasks out of it.
    #[test]
    fn a_fenced_block_is_content_rather_than_structure() {
        let page = "## Today\n\n- [ ] real <!--t:7k2p since:2026-08-19-->\n\n```markdown\n## Today\n- [ ] an example, not a task\n```\n\n## Next\n";
        let board = board_of(page);

        assert_eq!(board.tasks().count(), 1, "only the line outside the fence");
        assert!(board.unstamped().is_empty(), "an example is never adopted");

        let rendered = board.render(Locale::En, date(2026, 8, 19));
        assert!(rendered.contains("```markdown"), "{rendered}");
        assert!(
            rendered.contains("\n## Today\n- [ ] an example, not a task\n```"),
            "the fenced lines survive exactly as written:\n{rendered}"
        );
        assert_eq!(
            Board::parse(&rendered).render(Locale::En, date(2026, 8, 19)),
            rendered,
            "and the page is still a fixed point"
        );
    }

    /// CommonMark's own rule, followed exactly rather than approximated: a fence closes on the
    /// same character, at least as long, with nothing after it.
    #[test]
    fn a_fence_closes_only_the_way_the_grammar_says() {
        let opened = Fence::opened_by("````rust").expect("four backticks open a fence");
        assert!(!opened.closed_by("```"), "a shorter run is content");
        assert!(!opened.closed_by("~~~~"), "another character is content");
        assert!(
            !opened.closed_by("```` still open"),
            "an info string cannot close"
        );
        assert!(opened.closed_by("````"));
        assert!(opened.closed_by("`````"), "a longer run closes");
        assert!(Fence::opened_by("``").is_none(), "two is not a fence");
        assert!(Fence::opened_by("- [ ] a task").is_none());
    }

    /// An unclosed fence swallows the rest of the page as content rather than reading half of
    /// it as structure — the page is malformed either way, and keeping it whole is the answer
    /// that loses nothing.
    #[test]
    fn an_unclosed_fence_keeps_what_follows_it() {
        let board = board_of("## Today\n\n```\n- [ ] inside forever\n## Next\n");
        assert_eq!(board.tasks().count(), 0);
        assert!(board.unstamped().is_empty());
        let rendered = board.render(Locale::En, date(2026, 8, 19));
        assert!(rendered.contains("- [ ] inside forever"), "{rendered}");
        assert!(rendered.contains("## Next"), "{rendered}");
    }

    /// A nested checkbox is a sub-step of the line above it. Promoting it would take it out
    /// from under the thing it belongs to, and the next render would put it back at column zero.
    #[test]
    fn an_indented_checkbox_belongs_to_the_line_above_it() {
        let board =
            board_of("## Today\n\n- [ ] a <!--t:7k2p since:2026-08-19-->\n  - [ ] first step\n");
        assert_eq!(board.tasks().count(), 1);
        assert!(board.unstamped().is_empty());
        assert!(
            board
                .render(Locale::En, date(2026, 8, 19))
                .contains("  - [ ] first step"),
            "and it keeps its indentation"
        );
    }

    /// A line typed with another bullet is a task the person meant; keeping it as decoration is
    /// the one outcome that helps nobody.
    #[test]
    fn every_bullet_the_grammar_allows_opens_a_task() {
        for bullet in ['-', '*', '+'] {
            let board = board_of(&format!("## Today\n\n{bullet} [ ] typed by hand\n"));
            assert_eq!(
                board.unstamped(),
                [(TaskState::Today, "typed by hand".to_string(), false)],
                "bullet `{bullet}`"
            );
        }
    }

    #[test]
    fn a_title_holding_a_comment_keeps_it() {
        let board = board_of(
            "## Today\n\n- [ ] fix <!--the old parser--> path <!--t:7k2p since:2026-08-19-->\n",
        );
        let task = board.tasks().next().expect("a task");
        assert_eq!(task.title, "fix <!--the old parser--> path");
    }

    /// Two lines of a paragraph are one paragraph. Writing a blank line before each of them
    /// would make them two, which is a change to what the person wrote rather than to how it is
    /// laid out.
    /// File order is the only priority this board has: the line at the top of Today is the
    /// thing being done now. Re-adding an adopted line would drop it to the bottom.
    #[test]
    fn a_line_typed_at_the_top_stays_at_the_top() {
        let mut board = board_of(
            "## Today\n\n- [ ] typed above everything\n- [ ] already stamped <!--t:7k2p since:2026-08-19-->\n",
        );
        board.adopt_unstamped(|state, title, done| {
            let mut task = Task::new("3b8q".parse().unwrap(), title, state, date(2026, 8, 19));
            task.done = done;
            task
        });
        let order: Vec<&str> = board.tasks().map(|task| task.title.as_str()).collect();
        assert_eq!(order, ["typed above everything", "already stamped"]);
    }

    /// A blank line between two paragraphs is part of what was written; one after a task line
    /// is layout this render decides.
    #[test]
    fn a_blank_line_dividing_prose_survives_and_never_doubles() {
        let page = "## Today\n\n- [ ] a <!--t:7k2p since:2026-08-19-->\n\n> first paragraph\n\n> second paragraph\n";
        let rendered = board_of(page).render(Locale::En, date(2026, 8, 19));
        assert!(
            rendered.contains("> first paragraph\n\n> second paragraph"),
            "{rendered}"
        );
        assert_eq!(
            Board::parse(&rendered).render(Locale::En, date(2026, 8, 19)),
            rendered,
            "and it is still a fixed point"
        );
    }

    #[test]
    fn a_note_above_the_first_section_keeps_its_shape() {
        let page = "# Tasks\n\nwhat this board is for\nand how I use it\n\n## Today\n";
        let rendered = board_of(page).render(Locale::En, date(2026, 8, 19));
        assert!(
            rendered.contains("what this board is for\nand how I use it\n"),
            "{rendered}"
        );
        assert_eq!(
            Board::parse(&rendered).render(Locale::En, date(2026, 8, 19)),
            rendered
        );
    }

    #[test]
    fn an_empty_board_still_offers_every_section_to_drop_a_line_into() {
        let rendered = Board::empty().render(Locale::En, date(2026, 8, 19));
        for state in TaskState::ALL {
            assert!(
                rendered.contains(&format!("## {}", state.heading(Locale::En))),
                "{rendered}"
            );
        }
        assert_eq!(Board::parse(&rendered).tasks().count(), 0);
    }

    #[test]
    fn a_task_moves_between_sections_keeping_everything_else() {
        let mut board = Board::empty();
        let mut task = Task::new(
            "7k2p".parse().unwrap(),
            "a",
            TaskState::Next,
            date(2026, 8, 17),
        );
        task.due = Some(date(2026, 8, 20));
        board.insert(task);

        let id: TaskId = "7k2p".parse().unwrap();
        let moved = board.move_to(&id, TaskState::Today).unwrap();
        assert_eq!(moved.state, TaskState::Today);
        assert_eq!(moved.due, Some(date(2026, 8, 20)));
        assert_eq!(board.get(&id).unwrap().state, TaskState::Today);
    }

    #[test]
    fn moving_a_task_the_board_does_not_hold_is_an_error() {
        let mut board = Board::empty();
        let absent: TaskId = "7k2p".parse().unwrap();
        assert!(matches!(
            board.move_to(&absent, TaskState::Today),
            Err(TaskError::Absent(_))
        ));
    }

    #[test]
    fn a_rendered_board_round_trips_through_its_own_frontmatter() {
        let mut board = Board::empty();
        board.insert(Task::new(
            "7k2p".parse().unwrap(),
            "a",
            TaskState::Today,
            date(2026, 8, 19),
        ));
        let once = board.render(Locale::Ko, date(2026, 8, 19));
        let twice = Board::parse(&once).render(Locale::Ko, date(2026, 8, 19));
        assert_eq!(once, twice, "a board is a fixed point of parse-then-render");
    }
}
