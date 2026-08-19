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
- **Suppressing and REPORTING ask different questions of the same record.** `is_answer()` —
  finished or dropped — is right for suppression: both mean the observation was dealt with. It is
  wrong for the report, because a DROP is a decision that stands, and naming it every morning
  asks a person to write down again what they deliberately said no to. Counted together the line
  grew with every correct use of `lore task drop` until the remedy it named was wrong for most of
  what it counted. `Answered` holds both sets from one pass over the history: `contains` answers
  the suppression, `is_recoverable` answers the report — finished, and not since declined.
- **A proposal needs no store of its own.** Three things already settle whether an observation
  has been dealt with: the BOARD holds what is open however it got there, the HISTORY holds a
  completion or a drop (`TransitionKind::is_answer`), and a source that no longer declares it
  is not offering it. `lk_core::origin::identity` is the join — `blake3(url)[..16]` on the
  stamp's `src:`, a hash because a stamp value is `[0-9A-Za-z-]+` and a URL is not, and an
  identity because the visible link lives in a title a person is expected to rewrite. The one
  gap is a proposal DELETED in an editor rather than dropped, which returns tomorrow; that is
  the same silence deleting any task line already has, and a proposal that comes back costs
  less than one suppressed by a rule guessing at what a deletion meant.
- **A store damaged in ONE place does not cost the rest their day.** A snapshot or a judged
  file that will not read is named and skipped, and left where it is; the run proceeds with what
  it could read. Refusing everything blocked every proposal for as long as one hidden file sat
  there — nightly, with no path back — while dropping the file would lose judgments nobody has
  seen. This is the isolation an adapter already uses when one feed of many is broken, and it
  stops exactly where nothing was reached. The TRANSITION LOG is the exception at every level:
  a line that will not read and a filename that is not a date are both hard errors there,
  because passing over either re-proposes work the unreadable half says was answered.
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
- **A day that has not ENDED cannot be closed.** `--closing` left unbounded stamped a future
  `carried-on:` onto a managed page — the one thing a realized-only vault forbids — and poisoned
  the guard for every close after it, since a later real day compares as older.
- **A reminder about a task the board no longer holds open is moot, however it left.** Finished
  or dropped, `commit` retires it at the moment the transition is recorded and says so. SETTLED
  counts as left: a ticked line whose completion an earlier pass recorded writes no transition,
  so reading only what this pass recorded left the reminder standing for a task the board had
  just let go. Deleted in an editor, nothing is recorded and no completion could — so firing
  asks the board, which is the truth about what is open. One rule at each moment a reminder
  changes hands. Where it CANNOT decide — no board file at all, or a fence that never closes
  and hides every task — nothing is said and nothing is retired: reading what is due and
  retiring it in one call put the write before the decision, and a promise this store guarantees
  is late became one that was gone.
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
- **An appointment is reported, never proposed, and its store is keyed by SOURCE.**
  `SourceDescriptor::scheduled` marks a source whose items are times already committed to. `lore
  agenda` shows them beside the day's tasks and the board never learns of them: a meeting
  happens whether or not a line is cleared. Keyed by DATE the snapshot could not say a day was
  cleared — a date with no events produced no entry, so the writer was never called for it and a
  day whose every meeting was cancelled went on showing them. Keyed by source, cancelling them
  all is an empty snapshot, which is an answer; and a snapshot holds one window rather than one
  file per day forever, so there is nothing left for a retention horizon to prune. What it holds
  comes off the FETCH rather than off the rendered events, for the reason `open_work` does:
  `lore ingest --date <past>` narrows what is rendered to one day, and a whole-snapshot write
  from that blanked every other day the calendar holds.
- **Only a completion is an observation.** A dropped task belongs in the history and not on a
  daily page: the archive answers "what did I do", and deciding not to do a thing is not doing
  it. `TransitionKind::is_observation` is the one place that judgment lives.
- **The plane's stores answer four questions once, not once each.** `store::Jsonl` and
  `store::Shelf` hold what every one of them needs — read a JSONL file, treat an absent one as
  empty, refuse a line that will not parse naming the file and the line, replace the whole file
  atomically — because four copies is four places for those answers to drift and they already
  had. What each store still owns is its SHAPE and its LIFETIME: one file or a shelf of them,
  replaced or accumulated, and whether anything ever retires it. An empty `replace` writes an
  empty FILE, which for a snapshot is the answer that retires what the last run declared;
  `retire` removes it, which is a different statement.
- **Each store's lifetime is declared, and `lore maintenance` reads the declaration.** The
  TRANSITION LOG is knowledge — a completed task becomes it, `lore ingest --date <past>`
  reproduces a day from it, and never pruning is what makes "have I answered this observation
  before" exact rather than bounded by a horizon. The proposal snapshots and the standing
  reminders are STATE: pruning them would retract what a source says is open and what a person
  asked to be told. The day's SCHEDULE is neither — a rendering aid whose durable record is the
  calendar's own daily page — so it is operational history and prunes on the ingest log's
  horizon. Without that it gained a file a day, forever, for a view that asks about one.
- **A snapshot no configured source answers for is not read, and `enabled: false` is not
  removal.** A source deleted from `config.yaml` leaves a file that can never be read again, and
  a file nothing can read is not state — `lore maintenance` removes it. Disabling a source is a
  PAUSE: its snapshot is kept for the day it comes back, so the sweep asks what is configured
  while the read asks what is enabled. The same rule reaches the judged candidates through
  `propose_from`, because validating a judgment only when it is WRITTEN answers for the moment
  it was made and not for the moment it is acted on — a file five months old was still putting
  work from a dropped source onto the board. A file is settled by ROW: what was taken is
  written out of it and what was passed over stays, and `add` keys by DATE so one day's file
  holds every source's judgments.
- **The board file is the TRUTH, not a rendering of a store kept elsewhere.** The vault is the
  product: a box ticked on a phone has to count, and a design that keeps state somewhere else
  discards that edit in silence — worse than any parsing risk. What keeps the parsing risk small
  is that a line this cannot read is never rewritten: a stamp that will not parse is kept
  verbatim, reported through `Board::malformed`, and left out of every rule, because the bytes
  belong to whoever wrote them and a guess at what a corrupted date meant is a task silently
  rescheduled.
- **A heading this does not read CLOSES the section it follows.** That is what a heading at
  the sections' own level or above means in the document, and the board's whole state machine
  rests on the heading being the state — so `## Blocked` written under `## Next` opens something
  no state answers to, and every line beneath it belongs to no section. Read as ordinary content
  the block was annexed by the heading above it in silence: a person's blocked work was reported
  as ready and carried by the day-close, a hand-typed line there was adopted into the wrong
  state, and a proposal parked under it went on being proposed every morning because `select`
  could not see it. Named rather than interpreted, and the lines are held where they were found
  so the page comes back out byte-identical — being told costs the person nothing, and
  `## Waiting` is the state they were reaching for. A DEEPER heading is a subdivision and changes
  nothing, because `### 오전` under `## Today` is how a person organizes a section rather than
  how they leave one. The parse already answered this for a checkbox before the FIRST heading;
  it was the same question with the answer written in only one of the two places.
- **State is the heading a line sits under.** `Board::retag` assigns it after the walk rather
  than reading it off the stamp, so dragging a line between two headings in an editor IS the
  state change and a stale stamp cannot undo it. This is what keeps the whole plane usable by
  someone who never runs the command line.
- **Every field whose SHAPE is known is parsed into it.** `t:` into a `TaskId`, the dates into
  dates, `carried:` into a count — and `src:` was the one that was not, kept as whatever the
  field grammar allowed. A `src:` cut short by a crash mid-write or a sync client's conflict
  stayed a legal field, so the line parsed as an ordinary task carrying an origin that names no
  observation: invisibly, with nothing reported anywhere, and permanently the moment `done` or
  `drop` wrote that value into an append-only date file. The work then read as never answered
  and its source offered it again every morning. Refused now like a broken date, which makes it
  a named line awaiting repair rather than a silent one.
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
- **A task on the page that the parse could not PLACE is one fact, not four — and the LINE is
  kept, not just its number.** A stamp it cannot read, a line dragged above the first heading, a
  heading of the person's own with tasks under it, an indented line, one of Obsidian's alternate
  states (`- [/]`, `- [-]`, `- [>]`) which its default theme renders and a person reaches for
  while working, a fence that never closed and swallowed the rest — each was found on its own
  and guarded against on its own, by whole-board proxies that a per-line case walks straight
  past. `Board::unplaced` is the question the parse can already answer, and it is what makes
  "this id is not on the board" an ANSWER rather than a silence: finding an id is proof and
  needs no guard, while not finding one means something only where every checkbox the page holds
  was placed. Not INTERPRETED — what `- [-]` means is a judgment — but named, so the person can
  put the line back and get the task whole with its carry count and origin. Keeping only the
  TEXT of a stamp that would not READ was the same mistake one level down: an unplaced line
  still names a task, and both rules that reach a task through what the page already claims were
  blind to it — `Board::ids`, so the mint could hand the same address out twice and repairing
  the line would produce two live tasks answering to one, and `Board::origins`, so a proposal
  parked under a heading of the person's own was offered again every morning about work already
  sitting on the page. One record with the line, the text and the REASON; `stamped` is the one
  place a field is mined out of a line, under all three questions. A CLOSED fence's examples are
  placed correctly and are not counted — the board's own format reference is full of them, and
  counting one would make every task-linked question permanently unanswerable — but a line
  carrying an address this MINTED can only have reached a fence by being one of this tool's own,
  parked behind one, pasted back in, or wrapped by a plugin, so it is named whether the fence
  closes or not. Cleared with the rest when the fence closed, the tidier the person's markup the
  quieter the loss. Reported at every read and carried onto the front door's row, grouped by reason and never summarised away: one
  sentence listing every way it can happen made the reader work out which was theirs, and a line
  whose repair goes unsaid is one nobody repairs.
- **"Is this our stamp" is asked of the two KEYS, and of NEITHER value.** `t:` and the `since:`
  `render_task` always writes beside it, both present — not the two characters `t:`, and not a
  `t:` that parses. Asked of `t:` alone, any comment a person wrote holding a `t:`-prefixed token
  was this tool's: `- [ ] call the vendor <!-- t:30 min -->` was quarantined and told its author
  that `30` is not a task id, a machine reference they never wrote; a line of prose carrying
  `<!-- t:5pm -->` degraded the front door's board row; and because the same signal decides
  whether the parse was COMPLETE, one stray note left every task-linked reminder undecidable —
  neither fired nor retired, for as long as the line sat there. Asked of the VALUE, the fix
  reopened the hole it closed one field over: an id truncated, clipped by a nearby edit or
  mangled by a merge failed to parse, the whole comment stopped being ours, and the line was
  re-adopted under a fresh id with the damaged fragment folded into its visible title — ticked,
  a fabricated completion under that title reaching the archive. A value is exactly what damage
  destroys; the keys beside a broken one are what still say whose line it is, and `read_stamp`
  then names precisely what is wrong. The two questions the predicate answers pull opposite ways
  — "should `read_stamp` own this line" wants to be strict, "does this line NAME a task" wants
  to be as permissive as it can be without reaching a person's text — and they meet at the keys.
  What that leaves is a CLOSED comment holding only `t:` and four legal characters: `<!--t:cc01-->`
  with its `since` gone and `<!-- t:asap -->` differ in nothing but a space, and no rule reads a
  mind.
- **What an unclosed comment does not hold is not evidence.** A truncation destroys everything
  past the cut and lands mid-field as often as between two, so a comment nobody closed names a
  task on `t:` alone — `<!--t:bb01 sinc` is a stamp cut in half, not a person's note.
- **Only an address this MINTED says a line is one of ours.** A bare checkbox is not: a grocery
  list written above the first heading and a `## Reading list` of the person's own are ordinary
  uses of a page this board preserves verbatim by design. Named, they asserted a task that was
  not there and pinned the front door at `!` until the person stopped writing that way — a
  diagnosis whose only repair is to give up the page. It is the same principle the fence follows,
  and it was applied there and not at the other two sites.
- **A comment nobody closed still has a body: the rest of the line.** That is where a stamp goes
  when a write is truncated or a sync client resolves a conflict, and yielding no body for it
  made the damage invisible to every question here — the line read as one a person typed, was
  adopted under a fresh id, and the real task's address, first day, carry count and origin were
  gone, with the broken fragment folded into the new visible title. Ticked, a fabricated
  completion under that title reached the archive. Reading it is what lets a truncated stamp be
  RECOGNISED rather than reinterpreted; the two repairs differ, so the message names which.
- **A trailing comment is a STAMP only if it names a task, asked in BOTH branches.** A comment
  written the normal way — as the only trailing one, `- [ ] fix the bug <!--more-->` — went to
  the branch a first fix did not touch, where `split_stamp` handed it to `read_stamp` and the
  line was quarantined as "`more` is not a key:value": a person's own line refused for a stamp
  they never wrote. One question in both places, or the fix covers the example it was tested
  with and not the shape a person writes. With a real stamp EARLIER on the line the trailing
  note is an annotation on an addressed task, and is refused rather than orphaning it.
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
- **Which ended day a carry closed is RECORDED, not inferred from when it was written.** Asked
  of the day the transition landed in, two closes in one sitting declaring two different ended
  days — catching up after a few days away — read as one and the second was silently skipped.
  `Transition::closing` is the same fact the task's `carried_on` stamp holds, so the resume
  guard still covers a board write that failed after the log write, without the proxy. A record
  written before the field existed carries none and answers for no day: its absence is not the
  fact, which leaves one command's window exposed across the upgrade and nothing after it. The
  window reaches back to the closing day, because the transition answering it was written on
  whatever day the close first ran — at or after that day, and not necessarily today.
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
  The window starts at the earliest `since` among the board's TICKED tasks and the guard is then
  asked PER TASK against its own — the window finds a candidate closure, the task's own first day
  decides whether it is its. Asked once for the batch, one long-standing task ticked beside a
  newer one widened the window for both, and a completion recorded under a recycled id two weeks
  before the newer task existed settled it: the line left the board, its real completion reached
  nothing, and the pass reported the board caught up. The window is bounded by data rather than
  by a guessed horizon: an id is minted against the ids currently on the board, so a
  recycled id's previous owner left before this task was written down, and a completion recorded
  on or after this task's own first day is this task's. A board with no ticked line asks about
  today alone, which is every ordinary pass — and `floor` widens it for the CARRY question alone,
  never for this one: reading further back for a completion let an older life of a recycled id
  settle a live line, and made `sync` and `rollover` disagree about the same tick. "Was this
  task already carried FOR THIS ENDED DAY?" is keyed on the pair, so it needs no window of its
  own — and no per-task bound either, unlike the completion guard beside it: a carry for an ended
  day is recorded on or after that day, so a stale pair from a recycled id could only be reached
  by a task that entered `## Today` on or before it, and both lives being on the board then is
  what the mint already makes impossible. It is absorbed wherever the transition was written — a carry for another day cannot
  suppress this one, and asked of today's file alone a close retried on a later calendar day
  wrote a second carry for one day. The window has a start and NO ceiling, and it is read off
  the dates the record actually HOLDS rather than counted day by day up to today: a completion
  filed on a date ahead of this pass's today — a `vault.timezone` moved westward, two machines
  whose clocks differ on one vault — sat outside a window that stopped there, so a still-ticked
  line was harvested again and one completion reached two dates. Asking the shelf also makes a
  floor far in the past one directory read rather than an `open` for every day in between. A
  date file
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
- **A carry counts what the day BEGAN with, asked over two passes.** The committed set is
  captured before `sync` runs, because a task the same pass woke or adopted arrived during the
  close — stamping it `carried:1` would have it claim to have survived a day-close its own
  `since` says it never saw. That question only reaches THIS pass, and the day being closed is
  not the day the close runs on: a task written down or moved in this morning is already under
  the heading before the close reads the board, so the board alone calls it committed to
  yesterday. `Recorded::entered_today_after` is the other half — every route into `## Today`
  records a transition naming the state, so the history says on which day the commitment began.
  Without it the scheduled close that fires when the laptop wakes stamped `carried-on:` for a
  day the task did not exist on, and the count that exists to diagnose a stale task was wrong
  from its first day. It only ever REFUSES a carry and never invents one: a line dragged under
  the heading in an editor is a state change nothing records, and reading that silence as "never
  committed" would drop the carry of every task managed the way this board is meant to be.
- **A page a write cannot land on is reported when the plane is OPENED and refused at the
  WRITE.** Two different moments: reading is always safe, and a command touching none of the
  board — a reminder firing, a judgment being noted — has no business being turned away by a
  defect in a page it never opens. Coupled to one flag, an unterminated fence silenced the
  reminder timer, and the shipped script's process substitution swallowed the non-zero exit so
  the person got silence.
- **A view says what it could not SEE.** Answering empty on an unreadable store is the one
  shape refused everywhere else here: a caller — the front door's board row, the JSON a session
  acts on — cannot tell "nothing is promised" from "I lost your promises", and the second is the
  one that needs saying. Every one of them: `unrecorded`, `done_today`, the schedule and the
  reminders each answer `null` rather than empty, the front door's row says the record cannot be
  read, and the reason goes to stderr where it cannot corrupt the contract on stdout. Fixing one
  of four left three saying the reassuring thing.
- **Every writer of the plane holds it, or REFUSES.** `PlaneLock` on
  `<vault>/.lorekeeper/tasks.lock`, from before the plane is read until after it is written. It
  lives in `lk-task`, beside the stores it guards, rather than beside the first command that
  needed it: named for one writer it guarded `lore task` while `lore ingest` wrote the proposal
  snapshots and `lore maintenance` rewrote judged rows beside it, which is the same defect it
  already had once when it was named for the BOARD rather than the plane. `lore ingest` holds it
  for each plane write and no longer — held across a fetch that takes minutes it would lock a
  person out of their own board for those minutes. The board and the log are each a
  read-modify-write, so the scheduled day-close and someone closing a task by hand could both
  read, both write, and drop one of the two — and the completion that disappeared was gone from
  the history, which is the only thing the archive reads. `File::lock` BLOCKS until it is
  granted, so a failure is never contention: it is a plane that can never be held. Proceeding
  there lost board lines and records while reporting success — eight concurrent adds left two
  lines and four records — so it is refused, which is the standing rule wherever this tool
  cannot keep a promise. `Unholdable` says WHICH of the two failures it is, structurally rather
  than by matching text in a message, because they have different repairs: a lock file that
  cannot be OPENED is something sitting at that path, and only `lock` itself failing means a
  filesystem with no locking. The lock is the kernel's, so a crash releases it with no staleness
  rule to get wrong; it cannot reach across machines, which is the same limitation an editor on
  either machine already has.
- **A READ never waits for the plane and never holds it — but it asks whether a write could.**
  `PlaneLock::is_holdable` answers with `try_lock`, which cannot block, and a plane another
  command holds this instant is HOLDABLE: reported otherwise, every busy moment would read as a
  broken vault. What it catches is the lasting failure, and it is what fills `unwritable` for a
  reader — the front door printed a quiet day and the contract said `unwritable: null` over a
  vault where the nightly close, every command and the reminder timer were all being turned
  away. Reading itself is never refused: every write goes through `write_atomic`, so a reader
  sees a whole file or the previous one.
- **A write refuses to land on a version it never saw.** `commit` re-reads the page and compares
  it to the bytes this command parsed, BEFORE the log write, so an edit that arrived from an
  editor or a sync client in between is never erased — and a refusal leaves nothing recorded
  either, rather than a completion in the history for a task still sitting on the board.
- **A line on the board is a board WRITE, and every one of them records its transition.** A
  proposal appearing under `## Proposed` is a task coming into existence exactly as `add` and an
  adoption are, and it alone wrote nothing — so the history could not say how a proposed task
  came to be there, and its id sat outside `Recorded::seen` for as long as nobody answered it.
  A board written without its transition is something that happened and left no record.
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
