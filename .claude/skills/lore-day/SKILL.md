---
name: lore-day
version: 0.20.0
description: Run the user's working day off the Lorekeeper task board — what is on today, what the sources proposed, what an editor changed, what to remind them of, and closing the day so what they did becomes knowledge. The user speaks; this maps it to `lore task` and never asks them to type a command.
when_to_use: |
  오늘 뭐 해야 해, 오늘 할 일, 뭐부터 하지, 이거 해야 해, 이거 할 일로 넣어줘,
  그거 끝냈어, 다 했어, 이건 안 해도 돼, 내일 다시 알려줘, 3시에 알려줘,
  오늘 뭐 했지, 하루 정리, 마감하자, 리마인더, 제안 정리,
  what's on today, add a task, I finished that, drop that, remind me at,
  wrap up my day, triage the proposals
argument-hint: "[what the user said]"
allowed-tools: |
  Bash(lore *)
  Bash(date *)
---

# lore-day — the working day, spoken rather than typed

The user does not type `lore` commands. They say what happened and you turn it into the board's
truth. Every id in this document is read out of JSON, never out of a rendering.

## Always start by reading the day

```
lore agenda --json
```

That one call answers everything: `schedule` (appointments — reported, never actionable),
`committed`, `woken`, `due`, `proposed`, `reminders`, `done_today`, and `unrecorded`. Use its
`id` fields for every command below. Do NOT parse `lore agenda`'s human output — the `--json`
form is the contract and the columns are not.

If `unrecorded` is non-zero the user changed the board in their editor and nothing has recorded
it. Run `lore task sync` FIRST, then re-read, or you will act on a stale day.

## Mapping what they say

| They say | You run |
|---|---|
| "오늘 뭐 해야 해" | `lore agenda --json`, then tell them in their own words |
| "이거 해야 해" / a request in a thread | `lore task add <text> --state today` (`--link <url> --label <what it reads as>` whenever the thing came from somewhere) |
| "그거 오늘 할게" | `lore task move <id> today` |
| "그건 나중에" | `lore task move <id> next` (or `someday`) |
| "그거 끝냈어" | `lore task done <id> --note "<what it taught>"` — see below |
| "그건 안 해도 돼" / "다른 팀이 가져갔어" | `lore task drop <id>` |
| "내일 다시 보자" | `lore task wait <id> --until <YYYY-MM-DD>` |
| "3시에 알려줘" | `lore task remind add "<text>" --at 15:00 [--task <id>]` |
| "하루 정리하자" | close what is done with notes, then report what is left |

## The proposals are the point of the morning

`proposed` holds work the sources say is still open — a Jira issue assigned to them and not
done, a mail an earlier session judged to be a request. Each carries `origin`, the URL it came
from. Read them out, one line each, and ask for a decision on each. Then apply it:

- accept → `lore task move <id> today` (or `next`)
- decline → `lore task drop <id>` — this is what stops it being proposed again
- not now → `lore task wait <id> --until <date>`

Never leave a proposal sitting. It is a question, and an unanswered one is asked again tomorrow.
Do not decide for them: a proposal they never saw is worse than none.

## The note is the whole point of closing a task

`--note` is the only thing that reaches the archive, the concept extraction and every review.
A task closed without one leaves a title and nothing else.

So when they say they finished something, ASK what it taught — once, briefly, in their words —
and write what they answer. Not a status ("done", "완료"), not a restatement of the title: the
thing they now know that they did not this morning.

```
lore task done h3t6 --note "결제 타임아웃은 PG 가 아니라 커넥션 풀 고갈. max_idle 을 늘려도 재현됨"
```

If they have nothing to say, close it without a note rather than inventing one. A fabricated
note is worse than an empty archive — it becomes a concept page and compounds.

## A carried task is a diagnosis

`carried` counts day-closes survived. Past `personal.tasks.carry_warn_after` the agenda flags
it. When you see one, say so plainly: it is not asking for another day, it is too large or it
was never real. Offer to split it or drop it.

## Reminders

`lore task remind add "<text>" --at <HH:MM | YYYY-MM-DDTHH:MM>`. Times are read in the vault's
timezone, so pass the wall-clock time they said. Compute absolute times yourself — the flag
takes no relative form. `--task <id>` links it to a task when it is about one.

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

`lore task` writes what a person reads to stderr and machine output to stdout. Exit non-zero
means nothing was written — report the message rather than retrying.
