---
name: lore-ingest
version: 0.13.8
description: Daily knowledge ingestion pipeline — collects from Gmail, Google Drive, Google Calendar, Slack, Jira, RSS, and a manual inbox into an Obsidian vault. Deduplicates, classifies, extracts concepts, writes structured pages. Optionally tracks your own work into a work-log when the `personal:` module is configured. Idempotent, phased ingest with a no-data-loss guarantee.
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
| `lore ingest` | Collect ALL enabled sources, write pages; render the cross-source work-log when a `personal:` module is configured |
| `lore ingest <source>` | Refresh one source's pages only — never rewrites the work-log (it sees a subset) |
| `lore ingest --dry-run` | Preview without vault writes |
| `lore ingest --date YYYY-MM-DD` | Re-materialize a specific day (backfill / repair) |
| `lore synthesis weekly [--previous]` | Weekly synthesis + personal review |
| `lore synthesis monthly [--previous]` | Monthly performance review |
| `lore synthesis quarterly [--previous]` | Quarterly review with category stats |
| `lore synthesis annual [--previous]` | Annual review from quarterly reviews |
| `lore status` | Last time each source was COLLECTED — an empty run counts, so a quiet source shows its real timestamp with `0 events` |
| `lore health [--strict]` | Warn if any source is overdue vs `ingest.schedule` (2 missed fires; 48h fallback) |
| `lore performance` | Performance category distribution |
| `lore doctor` | Audit materialized pages against the text-cleanliness contract (non-zero on any defect) |
| `lore schedule --format launchd --pipeline-dir <dir>` | Print scheduled-task definitions. See the flag notes below — the bare form is rarely the one you want |
| `lore maintenance [--dry-run]` | Prune operational history (ingest log, drained queue files) past `maintenance.retention_days` (default 90d). Streaming event logs are permanent, and each source's latest log entry survives any horizon — it is the state `lore health` reads, not history |
| `lore queue prune [--dry-run]` | Leave the queue holding only work that still needs an LLM session: drop dead tasks (stale / missing-target), retire a run whose every task is already answered |

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
- "generate cron" / "schedule it" → `lore schedule` (read the flag notes first)
- "prune old logs" → `lore maintenance`
- "clean dead queue tasks" → `lore queue prune`
- "check the vault for defects" → `lore doctor`

## Scheduling flags

`lore schedule` with no flags emits cron entries running the bare subcommands. Both
defaults are usually wrong, and both fail SILENTLY:

- **`--format launchd` on macOS.** `StartCalendarInterval` runs a job missed while the
  machine slept as soon as it wakes; cron just skips it — and a closed laptop at 09:00
  is the normal case, so cron simply never ingests. Syntax launchd cannot express
  (`*/5`, `1-5`) is refused rather than approximated.
- **`--pipeline-dir <dir>`** points the ingest and weekly jobs at `lore-daily.sh` /
  `lore-weekly.sh`. `lore ingest` and `lore synthesis weekly` are only stage ONE of those
  scripts: the queue drain and `lore queue apply` live in the scripts alone, so without
  this flag the schedule ingests every morning and never fills a summary or materializes
  a concept. Only those two jobs take it — the janitors and monthly+ syntheses have no
  LLM stage.

Every emitted path must be absolute (`--bin`, `--pipeline-dir`): launchd searches no
`PATH` and expands no `~`.

## Output semantics

- Progress and diagnostics → **stderr**
- Data (cron lines) → **stdout**
- Exit codes: `0` success, non-zero on any failure. Findings-style commands
  have their own conventions: `lore health` exits `1` when a source is overdue,
  `lore graph *` uses `0` clean / `1` findings / `2` runtime error.

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
| Atlassian (Jira/Confluence) | `LORE_ATLASSIAN_SITE_URL` + `LORE_ATLASSIAN_PAT`, or `+ LORE_ATLASSIAN_EMAIL` + `LORE_ATLASSIAN_API_TOKEN` |

OAuth is not env-supplied: its refresh token rotates and must be written back, so `lore init credentials` owns it.

Default provider is `queue` (buffers tasks to JSONL for `/lore-process`).
`provider: noop` selects `NoopLlmClient` (no summarisation/concepts).

## Ingest flow

There is no commit step. A daily page is re-rendered in full every run, so any
write or flush failure just leaves the affected pages for the next run to reproduce
byte-identically (idempotent — that is the no-data-loss guarantee). Per-source
transactionality applies only to the LLM queue: each source opens a queue boundary
and a source whose plan fails rolls back its own buffered tasks, so the flushed
queue never references an unwritten page.

1. **Plan each source** — fetch, normalize, intra-batch dedup, classify. A source
   that fails is recorded and skipped; the run continues with the rest.
2. **Write pages** — atomic per file (tmp + rename). All daily sources
   (Gmail/Slack/Jira/Calendar/Drive/RSS) write `<daily>/{source-id}/DATE.md`; `manual`
   writes `<wiki>/documents/{slug}.md`. Concept pages are a cross-source aggregate,
   rendered once after all sources plan.
3. **Write work-log** — personal events only, and only when the optional `personal:`
   module is configured (a source must be in `personal.tracked_sources` AND match its
   adapter `is_self`; `manual`/RSS/Drive have no authorship, so never produce work-log
   entries even if listed). FULL ingest only: a source-filtered run skips this step —
   its event set is a structural subset and would overwrite the complete page.
4. **Flush LLM queue** — one atomic JSONL task file (queue mode).
5. **Archive** — `manual` inbox files move to `archived/{date}/`, only after every
   vault write and the queue flush succeeded, so a mid-run failure leaves them for retry.

In **queue mode** (the default), phases 1–5 leave summary/concept/work-log
sections empty and emit JSONL tasks. They are NOT knowledge yet — run
**`/lore-process`** afterward to drain the queue (fill summaries, create/merge
concept pages, work-log topic synthesis). Its own Finalize step reconciles the
graph and the generated wiki pages; that list lives there rather than being
restated here, because a partial copy of it is how the knowledge timeline went
stale. A daily run is `lore ingest` → `/lore-process`, not `lore ingest` alone.

## Safety notes

- Re-running is always safe — ingest is idempotent (pages are materialized views
  re-rendered in full from the source window), so a duplicate run just reproduces
  the same bytes. It's only wasteful, never corrupting.
- `lore ingest --date <past>` re-materializes a specific day — the way to repair or
  backfill a missing/edited daily page.
