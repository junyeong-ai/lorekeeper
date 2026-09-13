---
name: lore-day
version: 0.31.2
description: Run the user's working day off the Lorekeeper task board and the day's knowledge — what is on today, what the vault learned overnight, what the sources proposed, what an editor changed, what to remind them of, and closing the day so what they did becomes knowledge. The user speaks; this maps it to `lore task` and never asks them to type a command.
when_to_use: |
  오늘 뭐 해야 해, 오늘 할 일, 뭐부터 하지, 이거 해야 해, 이거 할 일로 넣어줘,
  어제 뭐 들어왔어, 새로운 소식, 읽을 거 있어, 이거 좀 더 설명해줘,
  그거 끝냈어, 다 했어, 이건 안 해도 돼, 내일 다시 알려줘, 3시에 알려줘,
  오늘 뭐 했지, 하루 정리, 마감하자, 리마인더, 제안 정리,
  what's on today, add a task, I finished that, drop that, remind me at,
  wrap up my day, triage the proposals
argument-hint: "[what the user said]"
allowed-tools: |
  Bash(lore *)
---

# lore-day — the working day, spoken rather than typed

The user does not type `lore` commands. They say what happened and you turn it into the board's
truth. Every id in this document is read out of JSON, never out of a rendering.

## Always start by reading the day

```
lore agenda --json
```

That one call answers everything: `schedule` (appointments — reported, never actionable),
`committed`, `woken`, `due`, `proposed`, `reminders`, `done_today`, `unrecorded`, `unplaced` and
`unwritable`. Use its `id` fields for every command below. Do NOT parse `lore agenda`'s human output — the
`--json` form is the contract and the columns are not.

If `unrecorded` is non-zero the user changed the board in their editor and nothing has recorded
it. Run `lore task sync` FIRST, then re-read, or you will act on a stale day. If it is `null`
the record could not be read at all — say so, and do not tell them the board is caught up.

If `unwritable` is not null, every change to the board will be REFUSED until the page is
repaired. Say so first, quote the reason — it names the line and the one-character fix — and do
not run any `lore task` command that writes until it is cleared.

If `unplaced` is not empty, the day above is INCOMPLETE — the page holds tasks no section could
place, so they are in no list, no carry and no archive. Say so before you report anything else,
naming each line and its `why` in their words, and offer the repair the reason implies (move it
under one of the four headings, outdent it, restore `- [ ]`). Never run a command against those
tasks: they have no id this can reach.

`null` ANYWHERE in the document means the store behind that key could not be READ, which is not
the same as empty or zero. Say so rather than reporting nothing promised, and read the command's
stderr — it names the file.

## Reporting the day is half of it; the other half is what is waiting on them

Two things in the document are WAITING FOR A DECISION, and both are raised every morning
without being asked. A person who has to remember to ask is a person whose backlog grows in
silence — which is what 42 unanswered proposals and a task carried seventeen times look like
from the inside.

- **`proposed`** — work the sources say is open that they have never answered. One line, not
  one line each: the count, the age of the oldest, and the offer. *"제안이 42건 밀려 있다.
  가장 오래된 건 8월 20일 것이다. 정리할까?"* The grouping comes on a yes, not before.
- **`committed` where `carried_too_long`** — a task that has survived more day-closes than the
  vault's threshold is not asking for another one. Name how many and offer the two real
  answers: split it, or drop it.

Raise them AFTER the day itself, so the report leads with what they actually have on. And
raise them as one line each even when the counts are large: the offer is what must not be
skipped, and the enumeration is what they will not read.

## The morning also has knowledge in it

```
lore brief --date yesterday --json
```

`learned` is what the vault did not hold before — each with the one line its page opens with.
`revisited` is what it already held and named again, and carries NAMES ONLY on purpose: the
reader knows those already, and restating them is the flood this reduces.

Both are read off the pages THAT DAY wrote — its daily pages, and the documents and
explorations created on it. So a concept no page cites appears in no day's brief, which is the
rule working rather than a loss: it is the orphan `lore graph lint` already reports. If the
user says something they captured is missing, that is what to check, not the brief. Read the learned
ones out in their own words, grouped as the answer groups them, and name the revisited ones
only when the user asks what else moved.

Ten lines, not thirty. If `learned` is long, say which groups it holds and read out the ones
whose category touches what is on their board today — the agenda is in hand, so that is a
judgment you can make and they cannot skim.

**A thin brief can mean a source stopped talking rather than a quiet day**, and the two read
alike from here:

```
lore health --json
```

Name any source whose `stale` is true, with `hours_ago` in days — *"my-wiki가 23일째 아무것도
못 읽고 있다"* — and say nothing at all when none is. It is a dead pipeline, not a quiet one,
and the knowledge half of this morning is wrong for as long as nobody knows. The command's
exit code is non-zero exactly when it has something to say, so a run that says nothing needs
no reading.

**When they want more, dig rather than summarize again.** `path` addresses the concept page:
read it, follow its `## 출처` to the daily pages behind the claim, and answer from those. A
question the vault cannot answer is a question to say it cannot answer — `lore wiki search
"<terms>" --json` says whether any page even reaches it — never one to fill from your own
background knowledge, which is how a vault stops being evidence.

Explain in the user's language, and explain the thing rather than the term: name what it is
for and what it changes before naming what it is called. A concept page's own sentence is a
definition written for the graph; a person asking about it wants to know why it is on their
screen.

## Mapping what they say

| They say | You run |
|---|---|
| "오늘 뭐 해야 해" | `lore agenda --json`, then tell them in their own words — and raise the two decisions above |
| "제안 정리" / "제안 봐줘" | group `proposed` and work through it, one group at a time |
| "어제 뭐 들어왔어" / "읽을 거 있어" | `lore brief --date yesterday --json` |
| "그거 좀 더 설명해줘" | read the concept page at its `path`, then its `## 출처` pages |
| "이거 해야 해" / a request in a thread | `lore task add <text> --state today` (`--link <url> --label <what it reads as>` whenever the thing came from somewhere) |
| "그거 오늘 할게" | `lore task move <id> today` |
| "그건 나중에" | `lore task move <id> next` (or `someday`) |
| "그거 끝냈어" | `lore task done <id> --note "<what it taught>"` — see below |
| "그건 안 해도 돼" / "다른 팀이 가져갔어" | `lore task drop <id>` |
| "내일 다시 보자" | `lore task wait <id> --until tomorrow` (or `<YYYY-MM-DD>`) |
| "3시에 알려줘" | `lore task remind add "<text>" --at 15:00 [--task <id>]` |
| "하루 정리하자" / "오늘 뭐 했지" | read `done_today` back to them, close what is still open with notes, then report what is left |

## The proposals are the point of the morning

`proposed` holds work the sources say is still open — a Jira issue assigned to them and not
done, a mail an earlier session judged to be a request. Each carries `origin`, the URL it came
from. Every one is a question they have not answered, so the morning raises the section whether
or not they ask for it. Then apply what they say:

- accept → `lore task move <id> today` (or `next`) — MOVE it; retyping the line by hand loses
  the origin and the source proposes it again tomorrow
- decline → `lore task drop <id>` — this is what stops it being proposed again
- not now → `lore task wait <id> --until <date>`

Never leave a proposal sitting. It is a question, and one they never saw is worse than none —
so do not decide for them.

An origin is offered ONCE. Finishing or dropping a proposal answers it for good, so a Jira
issue reopened weeks later never comes back as a proposal — a source declares what is open, not
what changed. When the user says something they already closed is live again, write it down
yourself with `lore task add "<what it is now>" --link <the same URL>`: the origin is the same
and the work is new.

**When there are many** — thirty open issues on a first run, or weeks of a morning nobody
answered — do not read thirty lines out. A wall of individual questions is how a person stops
reading the section, and the section only works if they read it.

Group them by the WORK, not by where it came from. A host tells you Jira from mail and nothing
else: nineteen mails are nineteen unrelated things, and "메일 19건" asks for a decision nobody
can make. Read the titles and put groups the person recognises — *"MDW 이슈 11건, GCP 알림 8건,
문서 검토 4건, 나머지 5건"* — with the oldest date on each, since age is what tells them which
group has been waiting.

**A group that is one thing REPEATED is not a group.** A condition still true is reported again
every day from a new address, so eight alerts about one broken log sink arrive as eight
proposals. Say it as what it is — *"같은 알림 8건"* — and offer the answer that fits: keep the
newest, drop the rest. One decision, and the work stays on the board.

Before applying a DROP to a group, name what is in it. Accepting a group moves everything and
loses nothing; dropping answers each origin for good, and one wrong member is work that never
comes back.

## The note is the whole point of closing a task

`--note` is the only thing that reaches the archive, the concept extraction and every review.
A task closed without one leaves a title and nothing else.

So when they say they finished something, ASK what it taught — once, briefly, in their words —
and write what they answer. Not a status ("done", "완료"), not a restatement of the title: the
thing they now know that they did not this morning.

```
lore task done h3t6 --note "결제 타임아웃은 PG가 아니라 커넥션 풀 고갈. max_idle을 늘려도 재현됨"
```

If they have nothing to say, close it without a note rather than inventing one. A fabricated
note is worse than an empty archive — it becomes a concept page and compounds.

`done_today` is the other side of this: every task closed today with its `note` and `carried`.
That is what answers "오늘 뭐 했지" and what a day's wrap-up is read from — the titles alone are
a list, the notes are what the day actually taught.

## A carried task is a diagnosis

`carried` counts day-closes survived, and `carried_too_long` is the judgment — the threshold is
the vault's, and this field is how you know it was crossed. When one is true, say so plainly:
it is not asking for another day, it is too large or it was never real. Offer to split it or
drop it.

## Reminders

`lore task remind add "<text>" --at <HH:MM | YYYY-MM-DDTHH:MM>`. Times are read in the vault's
timezone — the contract's `timezone` names it — so pass the wall-clock time they said. `--task <id>` links it to a task when it is
about one.

For anything but today, build the date from the agenda's own `date` — never from the machine's
clock. `date` is the VAULT's today and the machine's may be a different day, which is the whole
reason `--until` takes the words `today`/`tomorrow`/`yesterday`. The contract's `timezone` field
names the zone every time in the document is a wall-clock reading in.

A timer fires them (`lore-remind.sh`, installed by `lore schedule`). Do not run
`lore task remind due` yourself: it RETIRES what it prints, so calling it would consume a
reminder the user never saw.

## What this skill does not do

- It never runs `lore ingest`, the queue drain, or `lore task propose` — those are the scheduled
  pipeline's, and running them by hand mid-day duplicates work rather than adding any.
- It never edits `tasks.md` directly. Every change goes through a command, because the command
  is what records the transition the archive reads.
- It never invents a due date, a priority, or a note.

## Output semantics

`lore task` writes what a person reads to stderr and machine output to stdout. `lore task add`
prints the new task's id on stdout, which is what `--task <id>` and `move` take next.

A non-zero exit means the COMMAND failed, not that nothing happened. The history is written
before the board deliberately — a transition without its board move is re-derived by the next
pass, while a board written without its transition is work that left no record — so a failed
board write can leave the completion already recorded. Report the message; do not tell them
their work was lost, and do not re-run the same command to "make it stick".
