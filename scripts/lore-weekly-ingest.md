---
name: lore-weekly-ingest
description: Autonomous weekly Lorekeeper deepening — synthesise last week (cross-source themes + personal review), fill the synthesis queue, reconcile the graph, then run a knowledge audit (dormancy, contradiction worklist, near-duplicate review). Runs on Mondays after the daily ingest.
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
  review. Needs a full week of data; meaningless daily.
- **Knowledge audit** — dormancy (`lore graph stale`), the contradiction worklist
  (`lore graph audit-candidates`), near-duplicate merge candidates, and
  relationship-gap suggestions. These accumulate gradually; a weekly review keeps the
  worklist low-noise and the vault pristine without daily churn.

## Output contract (violation = task failure)

The final assistant text message MUST be one of:

- **(A) Success report** — weekly synthesis paths (cross-source + personal), queue
  tasks drained, graph-sync changed-page counts, and the audit summary: dormant-page /
  contradiction-candidate / near-duplicate counts, any contradictions flagged, and any
  merges recommended for human action. 5-10 lines.

- **(B) Partial failure** — which steps succeeded, which failed, and the recovery
  command (`lore synthesis weekly --previous`).

Idle termination with tool calls only and no text = failure.

## Procedure

### Step 1: Weekly synthesis

```bash
lore synthesis weekly --previous
```

- `--previous` synthesises last week (Mon-Sun) → `synthesis/weekly/` (cross-source
  themes) and `me/weekly/` (personal review) pages.
- In queue mode the pages are written with empty LLM sections plus queued
  narrative/theme tasks (the command flushes the queue file before returning).
- If last week has no data, the command writes nothing — report "no weekly data" and
  continue to the audit (Step 4).

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

### Step 5: Report

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
- Step 1 synthesis: re-run overwrites the same week's pages
- Step 2 lore-process: section replace + concept merge are idempotent
- Step 3 backlinks-sync / wiki index: deterministic, re-run is a no-op
- Step 4 audit: `audit-mark` is set-once-per-source-state and an already-flagged
  contradiction is not re-flagged, so a repeated run adds no duplicate callouts

Manual recovery:
```bash
lore synthesis weekly --previous
# then invoke /lore-process, then /lore-wiki audit
```
