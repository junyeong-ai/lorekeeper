---
name: lore-ingest
description: Daily knowledge ingestion pipeline — collects from Gmail, Google Drive, Google Calendar, Slack, Jira, and RSS into an Obsidian vault. Deduplicates, classifies, extracts concepts, writes structured pages. Tracks personal work for performance reviews. Atomic 5-phase ingest with no-data-loss guarantee.
when_to_use: |
  ingest, collect, daily ingest, weekly summary, monthly summary,
  quarterly review, annual review, performance review,
  health check, check status, schedule generation, cron,
  ai news, team digest, work log, concept extraction,
  vault ingest, dedup pruning, log rotation
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
| `lore ingest --date YYYY-MM-DD` | Backfill events for a specific date |
| `lore ingest --force` | Skip dedup, re-process everything |
| `lore synthesis weekly [--previous]` | Weekly synthesis + personal summary |
| `lore synthesis monthly` | Monthly performance summary |
| `lore synthesis quarterly` | Quarterly review with category stats |
| `lore synthesis annual` | Annual review from quarterly summaries |
| `lore status` | Last successful ingest per source |
| `lore health [--strict]` | Warn if any source stale (>48h) |
| `lore performance` | Work category distribution |
| `lore schedule [--bin <path>]` | Print crontab entries |
| `lore maintenance` | Prune ingest log + dedup cache (>90d) |

## Trigger mapping

- "ingest today" → `lore ingest`
- "ingest email only" → `lore ingest email-digest`
- "weekly summary" → `lore synthesis weekly --previous`
- "monthly review" → `lore synthesis monthly --previous`
- "quarterly review" → `lore synthesis quarterly --previous`
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

Five phases, all-or-nothing per run:

1. **Plan** — fetch, normalize, dedup-check, classify
2. **Write daily + concept pages** — atomic per-file (tmp + rename)
3. **Write work-log** — aggregate personal events across sources
4. **Flush LLM queue** — atomic JSONL task file (queue mode)
5. **Commit dedup** — only if all writes + flush succeeded

Crash between flush and commit → next run re-processes (no data loss).

## Safety notes

- Don't run if cron already scheduled (safe via dedup, but wasteful)
- Don't run `lore maintenance` concurrently (redb file lock)
- Don't `--force` casually (re-writes all pages)
