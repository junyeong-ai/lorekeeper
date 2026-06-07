---
name: lore-weekly-ingest
description: Autonomous weekly Lorekeeper deepening — synthesise last week (cross-source themes + personal review) plus any monthly/quarterly/annual review whose period just closed, fill the synthesis queue, reconcile the graph, run a knowledge audit (dormancy, contradiction worklist, near-duplicate review), then run the retention janitors. Runs on Mondays after the daily ingest.
---

## Role

Autonomous weekly deepening executor. Assumes no user present (scheduler fires
Monday morning, after the daily ingest has refreshed the catalog).

This task does NOT ingest sources — that is the daily task's job. It operates on
already-persisted pages: weekly synthesis reads last week's daily/work-log pages,
and the audit reads the accumulated vault. So it has no hard dependency on today's
daily run, though scheduling it after the daily run keeps the catalog and backlinks
current when the audit reads them.

### Cadence split

Daily (`lore-daily-ingest`) keeps the graph *structurally* consistent — backlinks,
catalog, broken links, invalid categories — with cheap deterministic passes that must
run the same day a concept is created. This weekly task adds the two things that
change slowly and would be noise (and LLM cost) run daily:

- **Synthesis** — a week of work compressed into cross-source themes and a personal
  review. Needs a full week of data; meaningless daily. The longer periods
  (monthly/quarterly/annual) ride this same cadence: each is an idempotent
  materialized view, so invoking it weekly costs nothing outside the first week
  after its period closes — no date-gating logic anywhere.
- **Knowledge audit** — dormancy (`lore graph stale`), the contradiction worklist
  (`lore graph audit-candidates`), near-duplicate merge candidates, and
  relationship-gap suggestions. These accumulate gradually; a weekly review keeps the
  worklist low-noise and the vault pristine without daily churn.

## Output contract (violation = task failure)

The final assistant text message MUST be one of:

- **(A) Success report** — synthesis pages written this run (weekly always;
  monthly/quarterly/annual when their period just closed), queue tasks drained,
  graph-sync changed-page counts, the audit summary (dormant-page /
  contradiction-candidate / near-duplicate counts, contradictions flagged, merges
  recommended for human action), and janitor prune counts. 5-10 lines.

- **(B) Partial failure** — which steps succeeded, which failed, and the recovery
  command (`lore synthesis weekly --previous`).

Idle termination with tool calls only and no text = failure.

## Procedure

### Step 1: Periodic synthesis

```bash
lore synthesis weekly --previous
lore synthesis monthly --previous
lore synthesis quarterly --previous
lore synthesis annual --previous
```

- `--previous` targets the period that just closed: last week (Mon-Sun) →
  `synthesis/weekly/` (cross-source themes) + `me/weekly/` (personal review); last
  month/quarter/year → `me/{monthly,quarterly,annual}/` reviews.
- Run all four EVERY week, no boundary check: each page is an idempotent
  materialized view, so outside the first week after its period closes the command
  re-renders the same bytes and queues zero LLM work — and in the week a
  month/quarter/year closes, its review materialises on its own.
- In queue mode the pages are written with empty LLM sections plus queued
  narrative/theme tasks (each command flushes the queue file before returning).
- If a period has no data, the command writes nothing — report it and continue.

### Step 2: Drain the synthesis queue

Invoke `/lore-process` (no arguments) to fill the weekly narrative and themes the
synthesis just queued. Without this the synthesis pages keep empty LLM sections until
the next daily run.

### Step 3: Reconcile the graph

```bash
lore graph backlinks-sync   # credit any concept citations the synthesis added
lore wiki index             # catalog the new synthesis pages
```

Both are deterministic and idempotent (diff-based / full rebuild) — re-running is a
no-op. This leaves the graph fully consistent before the audit reads it.

### Step 4: Knowledge audit

Invoke `/lore-wiki audit` (no arguments) for the weekly semantic review.

- It runs the dormancy check (`lore graph stale`), the contradiction worklist
  (`lore graph audit-candidates`), near-duplicate detection, and relationship-gap
  suggestions, then applies judgment to each.
- **Autonomous-safe.** The audit *surfaces and flags*, never destroys: it writes
  `> [!conflict]` callouts for genuine contradictions (reversible, in the LLM-owned
  synthesis body) and records `audit-mark` so a reviewed concept stays off the
  worklist until its sources change. Destructive consolidation (`lore graph merge`)
  is reported as a recommendation for human action, not auto-run. A missed week
  never corrupts the vault — it only defers review.

### Step 5: Retention janitors

```bash
lore maintenance   # prune ingest log, drained queue files, old streaming event logs
lore queue prune   # drop stale / missing-target tasks from the pending queue
```

Both are deterministic and idempotent; the weekly cadence keeps operational history
bounded without a separate scheduler entry.

### Step 6: Report

Output (A) or (B) as defined in the output contract.

## Guards

- No user questions — autonomous judgment only.
- No source ingestion here — this task never calls `lore ingest`.
- Turn budget 300s (synthesis + queue drain + audit is heavier than a daily run).
  The audit is best-effort within budget — `audit-mark` persists which concepts were
  reviewed, so any candidates not reached this week resurface next week rather than
  being lost. On timeout, report results up to the last successful step.

## Failure recovery

Every step is idempotent — safe to re-run:
- Step 1 synthesis: re-run overwrites the same period's pages (longer periods
  re-render byte-identically outside their first week)
- Step 2 lore-process: section replace + concept merge are idempotent
- Step 3 backlinks-sync / wiki index: deterministic, re-run is a no-op
- Step 4 audit: `audit-mark` is set-once-per-source-state and an already-flagged
  contradiction is not re-flagged, so a repeated run adds no duplicate callouts
- Step 5 janitors: prune past a retention horizon, re-run is a no-op

Manual recovery:
```bash
lore synthesis weekly --previous   # plus monthly/quarterly/annual --previous as needed
# then invoke /lore-process, then /lore-wiki audit
```
