---
name: lore-daily-ingest
description: Autonomous daily Lorekeeper ingest — refresh yesterday's pages (Monday → Friday), collect today's events, drain LLM queue, sync concept backlinks, and run a graph health check. Weekly synthesis + knowledge audit are a separate task (`lore-weekly-ingest`).
---

## Role

Autonomous daily ingest executor. Assumes no user present (scheduler
fires on weekday mornings).

Combines the `lore` binary + `/lore-process` skill:
- **Refresh yesterday** — re-ingest so Calendar scheduled→actual and Jira status changes are captured. No `--force`: Calendar/Jira are *mutable* source types and re-render with latest state automatically (`SourceType::is_mutable`); append-only sources (mail/Slack/RSS) correctly dedup as already-seen, so nothing is needlessly rewritten.
- **Ingest today** — lookahead events (Calendar 24h) + overnight activity (Slack/Gmail/Jira)
- **Drain queue** — `/lore-process` fills summaries, concepts, work-log topic synthesis
- **Graph health (daily)** — `backlinks-sync` re-derives `## 출처` + `source_count` from real citations; `lore wiki index` rebuilds the catalog; `lore graph lint` (read-only) surfaces orphans/broken/near-duplicate/conflict/invalid-category/index-drift

Weekly synthesis and the knowledge audit (dormancy, contradiction worklist,
near-duplicate review) are a separate cadence — see the `lore-weekly-ingest` task.

## Output contract (violation = task failure)

The final assistant text message MUST be one of:

- **(A) Success report** — dates processed, per-source event counts,
  queue tasks drained, `graph lint` finding counts, graph-sync changed-page
  counts. 5-10 lines.

- **(B) Partial failure** — which steps succeeded, which sources
  failed, recovery command (`lore ingest --force --date <date>`).

Idle termination with tool calls only and no text = failure.

## Procedure

### Step 1: Date calculation

```bash
TODAY=$(date +%Y-%m-%d)
DOW=$(date +%u)  # 1=Mon, 5=Fri

if [ "$DOW" = "1" ]; then
    YESTERDAY=$(date -v-3d +%Y-%m-%d)  # Monday → last Friday
else
    YESTERDAY=$(date -v-1d +%Y-%m-%d)  # Tue-Fri → yesterday
fi
```

### Step 1.5: Inbox binary file preprocessing

Scan `<vault>/inbox/` for non-text files and convert:

| Extension | Method |
|-----------|--------|
| `.pdf` | Read tool (pages param) → key summary |
| `.png` `.jpg` `.jpeg` `.webp` `.gif` | Vision → content description |
| `.docx` `.pptx` `.xlsx` | `pandoc {file} -t markdown` → full conversion |

Pandoc failure (encrypted, legacy OLE):
- Run `file {path}` to identify actual format
- Legacy `.ppt`/`.doc`/`.xls` → generate notice `.md`
- Encrypted → generate notice `.md`

For each file:
1. Write converted/summarised result to `inbox/{original-name}.md`
2. Move original binary to `inbox/archived/{date}/`

Text files (.md, .txt, .html, .json) are left for `lore ingest`.
Skip if inbox is empty or has no binary files.

### Step 2: Refresh yesterday's pages

```bash
lore ingest --date "$YESTERDAY"
```

- `--date` scopes to one day
- No `--force`: mutable source types (Jira/Calendar) re-render with the latest
  upstream state on their own; append-only sources dedup as already-seen, so the
  refresh updates exactly what changed and rewrites nothing else
- Continue to step 3 even on failure (record partial failure)

### Step 3: Ingest today

```bash
lore ingest
```

- All enabled sources (email-digest, my-schedule, my-tasks, team-slack)
- Calendar `lookahead_hours: 24` captures today's upcoming events
- Idempotent on same-day re-runs: mutable sources reflect latest state, append-only
  sources dedup; the LLM cache skips unchanged content either way

### Step 4: Drain LLM queue

Invoke `/lore-process` (no arguments).

Processes all `<vault>/.lorekeeper/queue/*.jsonl`:
- Generate summaries
- Extract concepts + create/merge concept pages
- Work-log cross-source topic synthesis

On success, queue files move to `queue/processed/`.

### Step 5: Backlinks + index rebuild + graph health check (daily)

```bash
lore graph backlinks-sync   # re-derive ## 출처 + source_count from incoming citations
lore wiki index             # rebuild wiki/index.md catalog from disk
lore graph lint             # read-only health report
```

- `wiki index` is a full deterministic rebuild: it adds the concept
  pages `/lore-process` just created and prunes phantom entries
  (catalog links to pages no longer on disk). Idempotent — re-running
  is a no-op. Daily cadence keeps the catalog current with the
  concepts created on every ingest.
- `graph lint` is pure read — no vault writes. Surfaces orphans, broken
  wikilinks, near-duplicate concepts, unresolved contradictions, invalid
  concept categories, and index drift. Dormancy (`graph stale`) and the
  contradiction worklist belong to the weekly task, not here. Exit code 1 means
  findings exist (NOT an error); capture the counts for the report and
  continue regardless.

- `backlinks-sync` is deterministic and idempotent (diff-based; only changed
  concept pages are written). It runs DAILY because it owns both the `## 출처`
  body AND the frontmatter `source_count` — ingest leaves the body empty and only
  approximates the count, so a daily sync keeps citations and counts current the same
  day a concept is created (a weekly cadence would leave new concepts with an empty
  Sources section and stale count for up to a week). Idempotency bounds the churn to
  genuinely-changed pages. Ingest preserves these sections across re-renders, so the
  two never fight.

### Step 6: Report

Output (A) or (B) as defined in the output contract.

## Guards

- No user questions — autonomous judgment only.
- Steps 2 + 3 are idempotent without `--force`: mutable sources (Jira/Calendar)
  re-render latest state, append-only sources dedup. Multiple runs per day never
  duplicate and never needlessly rewrite unchanged pages.
- No external communication (gh/SSH/etc.) after steps complete.
- Turn budget 180s. On timeout, report results up to the last successful step.

## Failure recovery

Every step is idempotent — safe to re-run on the same date:
- Step 2/3: mutable sources re-render latest state, append-only sources dedup
- Step 4 lore-process: section replace + concept merge are idempotent
- Step 5 wiki index: deterministic full rebuild, re-run is a no-op; lint is read-only
- Step 5 backlinks-sync: deterministic diff-based, re-run is a no-op

Manual recovery:
```bash
lore ingest --date <yesterday>
lore ingest
# then invoke /lore-process
```

`--force` (bypass dedup for ALL sources) remains available for the rare case of
rebuilding append-only pages from scratch, but the daily job never needs it.