---
name: lore-ingest
description: Daily knowledge ingestion pipeline — collects from Gmail, Google Drive, Google Calendar, Slack, Jira, RSS, and a manual inbox into an Obsidian vault. Deduplicates, classifies, extracts concepts, writes structured pages. Tracks personal work for performance reviews. Atomic phased ingest with a no-data-loss guarantee.
argument-hint: "<subcommand> [args]"
disable-model-invocation: true
allowed-tools: |
  Bash(lore *)
  Bash(crontab *)
---

# lore-ingest — Daily knowledge ingestion for Obsidian

Run the `lore` CLI for ad-hoc ingest, synthesis, and operations. The
CLI is config-driven (`./config.yaml` or `LORE_CONFIG`) and writes to
an Obsidian vault. All commands accept `--config <path>` to override.

## Command reference

| Command | Purpose |
|---------|---------|
| `lore validate` | Parse + validate config, print summary |
| `lore ingest [source]` | Collect from one source (or all enabled), write pages, aggregate work-log |
| `lore ingest --dry-run` | Preview without vault writes |
| `lore ingest --date YYYY-MM-DD` | Re-materialize a specific day (backfill / repair) |
| `lore synthesis weekly [--previous]` | Weekly synthesis + personal review |
| `lore synthesis monthly [--previous]` | Monthly performance review |
| `lore synthesis quarterly [--previous]` | Quarterly review with category stats |
| `lore synthesis annual [--previous]` | Annual review from quarterly reviews |
| `lore status` | Last successful ingest per source |
| `lore health [--strict]` | Warn if any source is overdue vs its schedule (2 missed fires; 48h fallback) |
| `lore performance` | Performance category distribution |
| `lore schedule [--bin <path>]` | Print crontab entries |
| `lore maintenance` | Prune ingest log + drained queue files (>90d) |

## Trigger mapping

- "ingest today" → `lore ingest`
- "ingest email only" → `lore ingest email-digest`
- "weekly review" → `lore synthesis weekly --previous`
- "monthly review" → `lore synthesis monthly --previous`
- "quarterly review" → `lore synthesis quarterly --previous`
- "annual review" → `lore synthesis annual --previous`
- "show performance" → `lore performance`
- "check status" → `lore status`
- "health check" → `lore health`
- "generate cron" → `lore schedule`
- "prune old logs" → `lore maintenance`

## Output semantics

- Progress and diagnostics → **stderr**
- Data (cron lines) → **stdout**
- Exit codes: `0` success, `1` stale sources, `2` runtime error

## Configuration

Required: `./config.yaml` or `LORE_CONFIG` / `--config`.

Relative `vault.root` resolves against the config file's parent
directory, not CWD.

Templates are embedded in the binary. Override with `--template-dir`.

## Credentials

| Source | Env vars |
|--------|---------|
| Google (Gmail/Drive/Calendar) | `LORE_GOOGLE_CLIENT_ID`, `LORE_GOOGLE_CLIENT_SECRET`, `LORE_GOOGLE_REFRESH_TOKEN` |
| Slack | `LORE_SLACK_TOKEN` |
| Jira | `LORE_JIRA_URL`, `LORE_JIRA_EMAIL`, `LORE_JIRA_TOKEN` |

Default provider is `queue` (buffers tasks to JSONL for `/lore-process`).
`provider: noop` selects `NoopLlmClient` (no summarisation/concepts).

## Atomic ingest flow

The atomic write→commit sequence is all-or-nothing per source, followed by a
post-commit cleanup hook:

1. **Plan** — fetch, normalize, dedup-check, classify
2. **Write page bodies** — atomic per-file (tmp + rename). Time-windowed
   sources (Gmail/Slack/Jira/Calendar/RSS) write `<daily>/{source-id}/DATE.md`;
   the `manual` source writes `<wiki>/documents/{slug}.md` instead (curated
   documents, not a dated feed). Concept pages are a cross-source aggregate,
   rendered once after all sources plan.
3. **Write work-log** — aggregate personal events across sources (only events a
   source-adapter marked `is_self`; `manual`/RSS/Drive have no authorship, so
   they never produce work-log entries even with `track_personal: true`).
4. **Flush LLM queue** — atomic JSONL task file (queue mode)
5. **Commit dedup** — only if all writes + flush succeeded
6. **Post-commit archive** — `manual` inbox files move to `archived/{date}/`,
   only after a successful commit, so a mid-run failure leaves them for retry.

Crash between flush and commit → next run re-processes (no data loss).

In **queue mode** (the default), phases 1–5 leave summary/concept/work-log
sections empty and emit JSONL tasks. They are NOT knowledge yet — run
**`/lore-process`** afterward to drain the queue (fill summaries, create/merge
concept pages, work-log topic synthesis), then `lore graph backlinks-sync` +
`lore wiki index` to reconcile the graph. A daily run is `lore ingest` →
`/lore-process` → graph sync, not `lore ingest` alone.

## Safety notes

- Re-running is always safe — ingest is idempotent (pages are materialized views
  re-rendered in full from the source window), so a duplicate run just reproduces
  the same bytes. It's only wasteful, never corrupting.
- `lore ingest --date <past>` re-materializes a specific day — the way to repair or
  backfill a missing/edited daily page.
