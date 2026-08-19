# lk-task

The intent plane: what the user means to do, and how it becomes what they did. Pure — no HTTP,
no async, no vault I/O beyond the transition log — so the rules are exhaustible by tests, and
the commands in `lk-cli` are the only thing that decides when to apply them.

- **Two joints and no more.** An observation may PROPOSE a task and never creates one; a
  completed task BECOMES an observation. `Transition::observation` hands the ingest pipeline an
  ordinary `RawItem`, so the daily page, the work-log, the contribution categories, the concept
  extraction and every review consume a completion without a line of them changing. That second
  joint is the entire archive — there is no archival machinery here, because finishing something
  is an event and this workspace already knows what to do with an event.
- **The FIRST joint, at last built: an observation proposes and never creates.** Two layers,
  because the two halves cannot be right in the same way. A source's STRUCTURED field answers
  "is this unfinished" with no reading of prose — a Jira issue assigned to the user whose
  `statusCategory` is not `done` — so no false positive is reachable and the layer needs no
  opting in; `RawItem::open_work` is where an adapter says so, the same discipline `is_self`
  follows. Free text has no such field: "does this mail ask me to do something" is a judgment,
  a rule over subject lines would fire on every newsletter, so it is made where judgments are
  already made and declared as one through `lore task candidate`, for the sources
  `personal.tasks.propose_from` names and no others. What lands is a `TaskState::Proposed` line
  and nothing more — accepting is dragging it into another section, declining is `lore task
  drop`, and both answers already existed.
- **A proposal needs no store of its own.** Three things already settle whether an observation
  has been dealt with: the BOARD holds what is open however it got there, the HISTORY holds a
  completion or a drop (`TransitionKind::is_answer`), and a source that no longer declares it
  is not offering it. `lk_core::origin::identity` is the join — `blake3(url)[..16]` on the
  stamp's `src:`, a hash because a stamp value is `[0-9A-Za-z-]+` and a URL is not, and an
  identity because the visible link lives in a title a person is expected to rewrite. The one
  gap is a proposal DELETED in an editor rather than dropped, which returns tomorrow; that is
  the same silence deleting any task line already has, and a proposal that comes back costs
  less than one suppressed by a rule guessing at what a deletion meant.
- **What re-declares gets a snapshot; what is observed once gets consumed.** A source that
  re-fetches its window can say what is still open every morning, so `Candidates` is replaced
  whole per source and a closed issue simply stops appearing — including the EMPTY write, which
  is what retires the proposals of a source whose work is all finished. An LLM session read one
  day's page and cannot re-declare yesterday's, so `Judged` candidates are consumed by the
  proposal that offers them; left to accumulate they would be re-read forever, and one deleted
  in March would return every day after.
- **A reminder is kept OFF the board.** It looks like it belongs there — forward-looking, the
  person's own — but a reminder is fired by a TIMER, and a timer that rewrites the board every
  few minutes writes their own file underneath an open editor and a sync client, on a schedule,
  forever; the kernel lock does not reach the other machine. So `Reminders` is its own store,
  where firing costs a write nobody is holding. What stays on the board is `wake:`, because that
  is a STATE CHANGE and belongs to the state machine — the two are not one idea at two
  resolutions. Firing retires it, for the same reason arriving clears a wake date: it was a
  promise to say something once. Nothing else retires one, so a reminder due while the machine
  slept is said late rather than lost.
- **A reminder about work that is finished is retired, at the moment it finishes.** A task
  leaves the board by being done or dropped, and the reminder someone attached to it is moot the
  same instant — so `commit` drops it there rather than leaving the timer to say it. A
  notification telling a person to do what they did this morning is the one failure that makes
  them stop reading notifications, and `lore task remind list` is read by a session deciding
  what to tell them. Best-effort and AFTER the board write: a reminder that outlives its task
  costs one misfire, while refusing would undo a completion that already happened.
- **One origin, ONE proposal — asked where the two stores meet.** Each store dedups inside
  itself, which is not the same rule: a Jira issue linked from a mail arrived as two candidates
  from two stores and became two lines about one piece of work. The set grows as the run goes,
  so a second candidate for an origin this pass just offered is caught too. A person answers
  about the WORK, and two decisions with one right answer is one decision too many.
- **An appointment is reported, never proposed.** `SourceDescriptor::scheduled` marks a source
  whose items are times already committed to. `lore agenda` shows them beside the day's tasks
  and the board never learns of them: a meeting happens whether or not a line is cleared, and
  putting one there would hand a person something to tick every morning for nothing.
- **Only a completion is an observation.** A dropped task belongs in the history and not on a
  daily page: the archive answers "what did I do", and deciding not to do a thing is not doing
  it. `TransitionKind::is_observation` is the one place that judgment lives.
- **The board file is the TRUTH, not a rendering of a store kept elsewhere.** The vault is the
  product: a box ticked on a phone has to count, and a design that keeps state somewhere else
  discards that edit in silence — worse than any parsing risk. What keeps the parsing risk small
  is that a line this cannot read is never rewritten: a stamp that will not parse is kept
  verbatim, reported through `Board::malformed`, and left out of every rule, because the bytes
  belong to whoever wrote them and a guess at what a corrupted date meant is a task silently
  rescheduled.
- **State is the heading a line sits under.** `Board::retag` assigns it after the walk rather
  than reading it off the stamp, so dragging a line between two headings in an editor IS the
  state change and a stale stamp cannot undo it. This is what keeps the whole plane usable by
  someone who never runs the command line.
- **The stamp READS everything and WRITES what this build knows** — the same rule the headings
  follow. A key `read_stamp` does not recognise is carried on `Task::extra` and re-emitted
  verbatim, because a board is one file two builds may open: a laptop updated this morning and
  a desktop that has not been, on a vault they both sync. Refusing an unknown field would take
  the whole line out of every rule on the older machine — not listed, not carried, not
  harvested — while looking perfectly normal in Obsidian the entire time. What is still refused
  is a pair that is not a pair, or a value outside the alphabet: unreadable rather than merely
  unknown.
- **Where a task came from goes in the VISIBLE title, as an absolute URL.** `lore task add
  --link` appends an ordinary markdown link, and three properties decide that shape. It is
  copied verbatim onto the archive page when the task closes, so a destination written relative
  to the board would resolve somewhere else from there and nothing on this path rewrites it. The
  board is a valid citation SOURCE (`lk_graph::scan::is_valid_source` includes `dirs.personal`),
  so a vault destination pointing at a concept would make that concept's evidence appear when
  the task was written down and DISAPPEAR when it was finished — against the rule that concept
  links only ever accumulate. And the vault is realized-only, so a link to a page a forward-
  looking task is about may not exist yet. `lk_core::link::is_external` keeps a scheme-bearing
  destination out of the graph entirely, which is what makes all three go away at once; a
  vault-relative `--link` is refused rather than rewritten.
- **Nothing inside a fenced code block is markup.** A board that documents its own format holds
  a `## Today` and a checkbox line as examples; reading them as structure deleted the heading
  outright and left the example to be adopted as a real task by the next reconcile. `Fence`
  follows CommonMark's close rule exactly — same character, at least as long, nothing after it —
  so it is the grammar rather than a guess at where code ends.
- **Headings are READ in every locale and WRITTEN in one**, like every other page in this vault.
  A `vault.locale` switch renames all four at once; a reader matching only the configured
  spelling would find no sections, treat every task line as prologue, and write them back under
  headings that hold nothing.
- **The machine half is an HTML comment with a strict alphabet.** It is the one form invisible
  in Obsidian's preview AND on GitHub, legal CommonMark, and stable across a round trip through
  both — the same test the vault's links are held to. Every stamp value is `[0-9A-Za-z-]+`, so
  nothing needs quoting and a value that would need it is a line this refuses to read rather
  than a parser with a quoting mode. `t:` is the identity because the TITLE is expected to
  change: refining what a thing actually is, is most of what keeping a list is, for the same
  reason a concept's slug is resolved once and never re-derived from its title.
- **`sync` answers the three things an editor can have done**, in the one order that is
  correct: adopt a checkbox typed without a stamp, then harvest a checkbox ticked, then wake a
  waiting task whose day has come. Adopting first is why a line typed AND ticked in one sitting
  records both facts instead of closing a task nothing created. A wake date is CLEARED by
  arriving — it was a promise to resurface once, and a task that kept one would be woken by
  every later pass.
- **A day closes once.** `rollover` takes the day's transitions and skips the carry for any
  task already carried in them, so the scheduled close and one run by hand do not both count —
  a task reading `carried:8` after four days is worse than no count at all. Asked of the day's
  own record rather than a marker written for the purpose, so a run that stopped partway resumes
  exactly where it stopped.
- **Adoption happens IN PLACE.** File order is the only priority this board has: the line at the
  top of Today is the thing being done now, and removing then re-adding an adopted line would
  drop it to the bottom on the next pass — which would also make "the file is the truth" false
  about the one thing the file says beyond its contents.
- **A blank line after any content is part of what was written**, a task line included —
  dropping it there made the note below read as a lazy continuation of that task's list item in
  every CommonMark renderer. Only a doubled blank and a trailing one are layout; trailing ones
  are trimmed at parse, which is what keeps the page a fixed point of parse-then-render rather
  than one that grows a line every time it is written back.
- **The generated title is the FIRST `# ` line and only that one.** Skipping every one of them,
  or guarding on an empty prologue, ate the heading a person wrote directly under it — which is
  where a person writes one.
- **`rollover` is `sync` plus the carry**, so the unattended day-close serves an editor-only
  user completely. Carrying is where most lists quietly fail — a task rolls forward untouched
  and forever, and the roll is invisible because nothing counts it. `Task::carried` counts it,
  and it counts day-closes rather than days since `since`: a task written down last month and
  committed to today yesterday is on its second day, not its thirtieth.
- **A ticked line always leaves the board; whether it is RECORDED is the separate question.**
  A completion the day's record already holds is a board write that did not land, so recording
  it again would put two of one finished task into the history — but leaving the line was worse
  than the duplicate it avoided: the board and the history disagreed until midnight, `list` and
  `agenda` kept reporting a finished task as open, and once the date turned the guard could no
  longer see the record, so the next pass harvested it onto a SECOND day, which two `EventId`s
  cannot collapse. Such a line is removed and reported as `settled`.
- **A day holds ONE completion per task, and `record` is where that is true.** An observation
  REPLACES the one the date already holds; every other kind is appended, because a task moved
  out of today and back is two moves and reading them as one would lose the day's shape. This is
  not a new rule — `Transition::observation` keys its item on `task:{id}` and `EventId` carries
  the date, so a second completion already collapsed downstream and only the record would have
  shown two. Stating it at the write is what lets a note reach a completion already written:
  `lore task done <id> --note` on a task closed an hour ago amends that transition rather than
  dropping the sentence, and the amendment is that transition plus the sentence, so the instant,
  the carry count and the title the day closed it under are the day's own.
- **Which day a close closes is DECLARED, not inferred.** `lore task rollover --closing` and the
  `carried-on:` stamp: a close run by hand at 23:00 and the scheduled one at 07:00 are two
  closes of one ended day, and their records live in two different date files, so each read the
  other's as empty and the count went up twice. The scheduled pipeline names the WORD
  `yesterday`, which `lore` resolves against `vault.timezone` — the zone every date in this tool
  is derived in. A `date -v-1d` in the script answers in the machine's zone instead, and on a
  host an hour the other side of the vault's that is a different day: the close then declares a
  day that never ended, and the declared day is the very key that stops one ended day being
  closed twice. Nothing else is dated this way — a harvested tick is stamped
  when it was DISCOVERED, because a checkbox carries no time and inventing one to make a page
  look better is the approximation this codebase refuses.
- **The history is asked TWO questions over two different windows.** `Recorded` holds both,
  because one list of transitions cannot answer them at once. "Has this completion already been
  recorded?" must look past midnight: a board write that fails in the evening is repaired by the
  next morning's pass, and asking only that morning's record found nothing, harvested the tick a
  second time, and archived one completion on two dates — which two `EventId`s cannot collapse.
  The window starts at the earliest `since` among the board's TICKED tasks, bounded by data
  rather than by a guessed horizon: an id is minted against the ids currently on the board, so a
  recycled id's previous owner left before this task was written down, and a completion recorded
  on or after this task's own first day is this task's. A board with no ticked line asks about
  today alone, which is every ordinary pass. "Was this task already carried?" must look at today
  alone: it is the guard that lets a run which stopped partway resume, and a carry seen from
  yesterday would suppress today's — the one number this plane exists to produce. A date file
  that will not read is FATAL to a write here rather than warned past, because the completion
  guard spans several of them and one read as an empty day harvests what the missing half
  already recorded. Inside that window a `Created` CLEARS the closure standing for its id rather
  than being compared to it: an id freed by a completion can be minted again, and then the
  earlier task's `Done` describes a task that no longer exists — so a line ticked for the NEW
  task reads as already closed and its completion is never recorded at all. Comparing instants
  at `Done` time does not settle that, because nothing retracts the credit once the next life
  begins; clearing settles it in one forward pass. And the mint takes the window's ids as well
  as the board's, which makes the recycle impossible there rather than merely survivable — a
  date's record holds one completion per task, so two tasks sharing an id on one date lose the
  earlier completion instead of duplicating it. The pass's OWN ids count too: `sync` takes the
  history by shared reference and so cannot record what it frees, and an id harvested a moment
  ago is off the board with its completion in `pending`, in no file the window read.
- **Answering from the history is for a line THIS PASS settled, not for an id the history knows.**
  `lore task done` on a task the board shows as OPEN closes it TODAY. Keyed on whether the
  window held any completion for the id, an editor undo or a sync client restoring an older page
  was enough to fold the work a person did today onto the day that older completion sits on —
  and no page was written for today at all.
- **Two lines answering to one id is reported, and refused before anything is written.** An id
  ADDRESSES a task and every rule here reaches a task through one, so a second line claiming it
  makes every rule ambiguous — and the ambiguity is settled by file order, which is not an
  answer. `Board::remove` takes the first line carrying the id: a copy placed ABOVE its ticked
  original had the wrong line deleted and the person's second task vanished with no transition
  in any file, while a copy placed BELOW it inherited the id, read as already closed on the pass
  that finished it, and its completion reached neither the history nor the archive. Duplicating
  a stamped line is how a person starts a similar task, so neither needs a collision in the
  mint. It is REPORTED rather than repaired — whether two lines with one address are two tasks
  or one task a sync conflict duplicated is a judgment about what the person meant, and
  re-minting the copy would silently double a board a merge duplicated — which is the standing
  `lore graph lint` has on two pages answering to one name. The message names the line and the
  one-character repair, and deleting the stamp from the copy makes the next pass adopt it.
- **A page whose code fence never closes is read and not written.** CommonMark runs an unclosed
  fence to the end of the document, so the parse is right and the page is wrong: every heading
  below that line is code, the render emits a fresh set of sections after them, and the next
  parse swallows those too — `lore task rollover` grew the board by six lines a night,
  unattended, with nobody touching the file. Opened above the first heading it is worse: no
  section is ever open, so every task is invisible to the tool while sitting in plain sight in
  the editor. So a read answers with what it parsed and says why, and a write is refused where
  the board is CLAIMED, before anything is half-done. Every line either diagnosis names is
  numbered in the FILE, not in the body the parse walks — the user goes and looks at it.
- **A pass asks the day's record before it records anything.** `sync` refuses to harvest a task
  whose completion the day already holds and `rollover` refuses to carry one it already carried,
  both read off the transitions rather than a marker written for the purpose — so a run that
  stopped partway resumes exactly where it stopped. Without the first, a board write that failed
  after the log write meant the next pass found the box still ticked and recorded a SECOND
  completion; the observation's `external_id` is `task:{id}` (the day is already in `EventId`)
  so even that collapses in the pipeline's own dedup rather than double-counting the work in the
  daily page, the work-log and the performance record.
- **A carry counts what the day BEGAN with.** The committed set is captured before `sync` runs,
  because a task the same pass woke or adopted arrived today — stamping it `carried:1` would have
  it claim to have survived a day-close its own `since` says it never saw.
- **Every mutating command holds a kernel lock** (`lk-cli`'s `IntentPlane`, `std::fs::File::lock`
  on `<vault>/.lorekeeper/tasks.lock`) from before the board is read until after the board and
  the log are written. The board and the log are each a read-modify-write, so the scheduled
  day-close and someone closing a task by hand could both read, both write, and drop one of the
  two — and the completion that disappeared was gone from the history, which is the only thing
  the archive reads. The lock is the kernel's, so a crash releases it with no staleness rule to
  get wrong; it cannot reach across machines, which is the same limitation an editor on either
  machine already has. Reading takes no lock: every write goes through `write_atomic`, so a
  reader sees a whole file or the previous one.
- **A write refuses to land on a version it never saw.** `commit` re-reads the page and compares
  it to the bytes this command parsed, BEFORE the log write, so an edit that arrived from an
  editor or a sync client in between is never erased — and a refusal leaves nothing recorded
  either, rather than a completion in the history for a task still sitting on the board.
- **`Reconciled` carries the transitions rather than writing them.** The rules stay a pure
  function of the board and the clock; `IntentPlane::commit` in `lk-cli` is the one place that
  touches the filesystem, and it writes the history BEFORE the board — a transition recorded
  without its board move is re-derived harmlessly by the next reconcile, while a board written
  without its transition is work that happened and left no record, and the log is the only thing
  the archive reads.
- **A page written on Windows still has frontmatter.** `strip_frontmatter` accepts `---\r\n`;
  matching only `---\n` took the whole page as body, so `id:`, `type:` and both fences came back
  as prologue lines under a freshly generated block, growing every time the page was written.
- **One log file per date, replaced atomically.** The same shape as the streaming sources' event
  log and for the same reason: a day's record is durable and complete on its own, so `lore
  ingest --date <past>` reproduces that day's archive exactly. A line that will not parse is a
  hard error rather than a skip — the writer only ever produces the file through an atomic
  replace, so damage is external, and dropping the line would let the next record rewrite the
  file without it.
