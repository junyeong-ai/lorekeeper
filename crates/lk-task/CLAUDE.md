# lk-task

Task intent and its durable stores. Follow the root CLAUDE.md for shared filesystem,
error, and testing rules. `board.rs`, `reconcile.rs`, and `task.rs` define board
meaning; `propose.rs`, `schedule.rs`, `remind.rs`, `log.rs`, and `lock.rs` own the
stores. CLI orchestration and `IntentPlane::commit` live in `lk-cli/commands/task.rs`.

## Observation is not commitment

- An observation may propose work; it never commits work to Today. A completion
  becomes a pipeline observation, whereas a drop stays only in task history.
- `Candidates::record` merges observed open work by URL and removes only explicitly
  settled URLs. A quiet, bounded fetch is not evidence that earlier work closed.
  Persist settlements until proposal reconciliation consumes them, so an already
  offered Proposed line can be withdrawn. Never withdraw an accepted task this way.
- Structured sources declare work; opted-in LLM judgment supplies the other proposal
  input. Keep their origins distinct, but grow one offered-origin set across both
  stores so the same work is not offered twice. The stamp origin is `lk_core::origin::identity` of the source URL; editing the
  visible title/link does not change the stored origin. Judged candidates are
  consumed when offered, unlike the merged structured-source candidates.
- Suppression and reporting differ: finished and dropped work answer a proposal;
  a recoverable completion is not a later decline. Consult board, history, and
  judgments rather than maintaining another proposal registry.
- Candidate/judgment files are independently recoverable: report and isolate damaged
  files while retaining other sources. Do not silently erase damaged bytes. History
  damage (including non-date filenames) is fatal to writes because history prevents duplicate completions.
- Schedule snapshots are keyed by source and replaced from the full fetched source
  snapshot, not the date-filtered rendered events. An empty schedule clears old
  appointments, not work proposals. Appointments are reported, never proposed.
- Store lifetime is declared by its owner for maintenance. Completion history is
  permanent; reminder/state snapshots are not age-pruned. Clean up sources removed
  from configuration, but `enabled: false` is a pause, not removal.

## Board parsing preserves the person's page

- The board Markdown is truth. State comes from the heading containing a line;
  moving a line in an editor changes its state. Read headings in all supported
  locales and render in the configured locale. An unknown heading at the same or a
  higher level closes the section; a deeper heading does not.
- Preserve malformed/unplaced stamped lines verbatim and report file line, text,
  and reason. Their claimed IDs/origins still participate in collision checks.
  A bare checkbox in an unknown section is not automatically one of our tasks.
- A closed machine comment is identified by the `t:` and `since:` keys, not by
  whether their values parse. For an unclosed comment, a minted `t:` key is enough
  to diagnose ownership; a closed `t:`-only comment is ambiguous. The remainder of
  an unclosed comment is still its body. Ordinary trailing comments are not stamps;
  a real stamp followed by prose/comment is refused rather than silently consumed.
- Parse known fields into typed values, retain unknown machine fields in `extra`,
  and round-trip them. Machine values use the strict stamp alphabet; alternate
  checkbox states do not acquire guessed meanings. A visible origin is an absolute
  external URL in the title, not an opaque field or internal page address.
- Respect CommonMark fenced blocks, including matching character, minimum closing
  length, and closing-line rules. Unstamped examples in closed fences are inert;
  stamped task lines remain unplaced diagnostics and reserve their IDs/origins even
  inside a closed fence. An unterminated fence permits diagnostic reads but refuses
  board writes. CRLF frontmatter is
  frontmatter too. Duplicate task IDs are reported and refused, never re-minted:
  removing the copied stamp is the explicit way to request adoption.
- Adopt unstamped tasks in place and preserve file order. Preserve meaningful blank
  lines and prologue prose; collapse only extra/trailing blanks. Only the first H1
  is the generated title, not every H1 on the page.

## Reconciliation and historical identity

- `sync` adopts unstamped lines, harvests ticked lines, then wakes due tasks.
  Clear a wake date once. `rollover` performs sync plus carry; editor-only usage
  must not require a prior explicit sync.
- A ticked settled line leaves the board even if its completion was already logged.
  A day stores one completion per task: amend that completion's note rather than
  append another observation. Preserve its original instant/title/carry information.
  `done` on an OPEN line closes it today; only a line this pass settled can amend
  a historical completion merely because history contains the ID.
- Closing day and record date are separate. CLI accepts only ended days in the vault
  timezone (`closing < today`). Older carry records without an explicit closing day
  do not prove a specific day closed. A carry is idempotent by `(task ID, ended day)`,
  wherever the transition was recorded, including a retry after midnight.
- Completion lookup starts at the earliest ticked task's `since`, then checks each
  task against its own first day. A carry floor must not widen that completion
  lookup. Read actual stored date files from the floor onward with no upper ceiling:
  clock/timezone differences can place a valid record after today's date. Any
  unreadable date file in that window is fatal to a write, not an empty day.
- A `Created` transition clears the prior life's closure for a recycled ID. Mint
  against IDs in the board, relevant history, and this pass's pending transitions;
  otherwise same-day completion upsert can erase a different task's completion.
- Carry only tasks committed before this pass's sync. Also consult
  `Recorded::entered_today_after` to exclude tasks that entered Today after the ended
  day. Missing history for an editor move cannot prove it was never committed;
  this guard refuses an incorrect carry, not invents one.
- Every board creation/move, including a proposal, records its transition.
  `Reconciled` returns transitions without IO; do not put persistence into the rules.

## Locking, publication, and read degradation

- Every intent-plane writer holds `PlaneLock` at `.lorekeeper/tasks.lock` from
  before read through write. Ingest holds it only for each plane update, never
  during a network fetch. Blocking acquisition errors are structural failures,
  not ordinary contention; preserve typed open-versus-lock failure diagnostics.
  Kernel locks are local coordination, not cross-machine exclusion.
- Reads never hold or wait for the plane. `is_holdable` uses a nonblocking probe;
  contention means holdable, not broken. Never probe while already holding the
  plane: closing a probe descriptor can release process-scoped locks on some
  platforms. An absent lock on a fresh vault is holdable without creating a file.
  Report lasting unwriteability separately
  while still returning readable data.
- CLI commit rechecks exact parsed board bytes BEFORE writing history, writes
  history BEFORE board, then retires moot reminders best-effort. This prevents
  overwriting editor changes and makes a partial commit safely reconcilable.
  Each date's JSONL is atomically replaced; a malformed history line is a hard error.
- Diagnose board write defects when opening the plane, but refuse them only if the
  operation actually writes the board. A reminder/judgment-only operation must not
  fail because an unrelated board fence is malformed.
- An unreadable store is unavailable, not empty. Read models expose `null` for
  unavailable `unrecorded`, `done_today`, schedules, or reminders, with diagnostics
  outside JSON stdout. Incomplete board parsing also prevents false absence claims.

## Reminders

Keep reminders off the board. Wake depends on board state; a late promise remains
until fired. Finished, dropped, explicitly settled, or deleted tasks make reminders
moot, but prove that against the board before retirement. Missing/unreadable or
incompletely parsed boards suppress both firing and retirement. Successful board
completion may retire reminders afterward best-effort; reminder failure must not
undo a durable completion.
