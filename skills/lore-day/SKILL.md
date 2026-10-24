---
name: lore-day
description: Manages the Lorekeeper task board and daily knowledge brief through conversational
  requests. Reads agenda JSON, records requested task changes and completion notes,
  and reports source freshness. Use for this board or its brief, not unrelated questions
  or external task services.
metadata:
  version: 0.92.2
---

# Lorekeeper working day

Translate the user's requests into board transitions. Read IDs and state from JSON;
never infer them from terminal columns or edit `tasks.md` directly.

## Read before acting

Start with `lore agenda --json`. Its date/timezone define the vault's day.

- For a read-only agenda or brief, report nonzero `unrecorded` and the indicated
  `lore task sync` action without running it. Before a requested board mutation,
  synchronize outstanding editor changes and reread the agenda.
  Null means the record could not be read, not that it is synchronized.
- Non-null `unwritable` blocks writes: report its reason and repair requirement
  before any board mutation. `unwritable: null` means no reported restriction.
- Nonempty `unplaced` means an incomplete day. Name the affected lines and reasons;
  they have no actionable IDs. Offer the indicated placement/markup repair.
- Null `schedule`, `reminders` or `done_today` means that store failed to read:
  report the limitation and diagnostic. Optional due/wake/origin/task fields can
  legitimately be null. Appointments in `schedule` are informational.

Report committed, woken and due work first. Raise outstanding proposals in one
line with count/oldest age and offer triage. For `carried_too_long`, report the
actual carry count and offer splitting, deferral or dropping; do not diagnose the
user's motives or invent a priority. Avoid flooding the user with every item.

## Knowledge brief

Run `lore brief --date yesterday --json` for the morning or a requested brief.
`learned` contains new concepts with an opening line; `revisited` deliberately
contains names only. Select relevant groups against the agenda and expand on request.
The brief selects daily pages by their address date and documents/explorations by
declared `created`. A cited concept's `created` separates learned from revisited.
This is not an ingestion or modification changefeed; uncited concepts are outside it.

Check `lore health --json`: a thin brief may reflect stale collection. Name stale
sources with the reported age. Health measures source freshness, not whether a
particular insight is true.

For a follow-up, use `/lore-wiki query` to read the concept and its evidence using
the schema's source-section name. Preserve the configured vault language and source
names. Say when the vault cannot answer; don't fill its gaps with an uncited claim.

## Requested transitions

| Request | Command |
|---|---|
| Add work for today | `lore task add <text> --state today` |
| Record where it came from | Add `--link <external-url> --label <readable-label>` |
| Commit / defer | `lore task move <id> today` / `next` / `someday` |
| Wait until a day | `lore task wait <id> --until tomorrow` or an explicit date |
| Complete | `lore task done <id> --note "<actual outcome or learning>"` |
| Decline or no longer needed | `lore task drop <id>` |
| Reminder | `lore task remind add "<text>" --at <time> [--task <id>]` |
| Wrap up | Read `done_today`, close requested remaining tasks with their actual notes, report what remains |

Use an outcome already provided. If none was given, ask once what the work taught;
if there is no note, close without one. Never invent a learning, deadline or priority.
Notes reach the completed-task archive and later extraction, so fabricated notes
would become fabricated knowledge.

`lore task add` returns its ID on stdout. Other diagnostic prose is not a machine
contract. A nonzero transition may already have recorded history before board
publication failed. Report the exact error; do not blindly repeat it or claim the
completion was lost.

## Proposal triage

A proposal is a decision for the user, not automatic commitment:

- Accept by moving its existing ID, preserving origin.
- Decline by dropping its ID; not now by waiting until a date.
- Group large sets by actual work, not source host. Include counts/oldest age.
  Repeated alerts may be one underlying task, but inspect them before proposing
  that the newest be kept and the others declined.
- Before dropping a group, identify its members and obtain that decision unless
  already authorized. Do not mass-decline merely to clear a backlog.

An answered origin is not proposed again. If the user says closed work is live
again, add the new work with the same external URL instead of expecting re-proposal.
Retyping an open proposal loses its original identity; move it.

## Reminders and scope

Pass times in the agenda's timezone. For another day, derive the date from the
agenda date rather than the machine clock. The scheduled `lore task remind notify` submits
reminders; never call `lore task remind due` here because it consumes what it prints.

Do not ingest, drain the semantic queue or run `lore task propose` during a board
interaction. Those belong to the collection workflow. Source service mutations
are outside this skill's task board.
