---
name: wiki-ingest
description: Daily knowledge ingestion pipeline that collects from Gmail, Google Drive, Google Calendar, Slack, and Jira into an Obsidian vault. Deduplicates events, classifies them with configurable labels and personal-work flags, extracts concepts via Claude, and writes structured markdown pages. Tracks personal work for weekly/monthly/quarterly/annual performance reviews. Atomic 4-phase ingest guarantees no data loss on partial failures. Read-only commands (status, health, performance) are safe to invoke any time; ingest and synthesis are typically cron-scheduled but can be triggered ad-hoc.
when_to_use: |
  ingest, 수집, 위키 수집, 정리, 업무 정리, 종합 보고, weekly summary, monthly summary,
  quarterly review, annual review, performance review, 성과 리뷰, 분기 리뷰,
  주간 정리, 월간 정리, 연간 정리, health check, 헬스 체크, 상태 확인,
  schedule generation, cron, 스케줄 생성, ai news, team digest, work log,
  업무 기록, concept extraction, 개념 추출, vault ingest, dedup pruning,
  log rotation, 메일 정리, 캘린더 정리, 슬랙 트렌드, jira 업무, 내 업무 추적
argument-hint: "<subcommand> [args]"
allowed-tools: Bash(wi *)
---

# wiki-ingest — Daily knowledge ingestion for Obsidian

Run the `wi` CLI for ad-hoc ingest, synthesis, and operations. The CLI is
config-driven (`./config.yaml` or `WI_CONFIG`) and writes to an Obsidian
vault. All commands accept `--config <path>` to override.

## Command reference

| Command | Purpose |
|---------|---------|
| `wi validate` | Parse + validate `config.yaml`, print summary |
| `wi ingest [source]` | Collect from one source (or all enabled), write daily/concept pages, aggregate work-log |
| `wi ingest --dry-run` | Preview ingest without vault writes |
| `wi ingest --date YYYY-MM-DD` | Backfill events for a specific date |
| `wi ingest --force` | Skip dedup cache, re-process everything |
| `wi synthesis weekly [--previous]` | Generate weekly synthesis page + personal weekly summary |
| `wi synthesis monthly` | Aggregate work-log into monthly summary |
| `wi synthesis quarterly` | Quarterly performance review with category stats |
| `wi synthesis annual` | Annual review from quarterly summaries |
| `wi status` | Last successful ingest per source |
| `wi health [--strict]` | Warn if any source is stale (>48h); `--strict` also fails on never-ingested |
| `wi performance` | Work category distribution from recent work-log pages |
| `wi schedule [--bin <path>]` | Print self-contained crontab entries (includes `--config`, `--previous`) |
| `wi maintenance` | Prune ingest log + dedup cache older than 90 days |

## Natural-language trigger mapping

When users ask in Korean, route to the appropriate command:

- "오늘 수집해줘" / "이메일만 수집해줘" → `wi ingest [source]`
- "지난주 정리해줘" / "이번주 업무 정리" → `wi synthesis weekly --previous`
- "지난달 요약" → `wi synthesis monthly --previous`
- "지난 분기 리뷰" → `wi synthesis quarterly --previous`
- "성과 보여줘" / "업무 분포" → `wi performance`
- "상태 확인" → `wi status`
- "헬스 체크" → `wi health`
- "스케줄 등록" / "크론 만들어줘" → `wi schedule` then paste into `crontab -e`
- "오래된 로그 정리" → `wi maintenance`

English equivalents: "ingest today", "weekly summary", "monthly review",
"check status", "show performance", etc.

## Output semantics

- Progress and human-readable diagnostics → **stderr**
- Data (cron lines from `wi schedule`) → **stdout**
- Exit codes: `0` success, `1` for `wi health` when stale sources exist
  (or when `--strict` and any source is never ingested), `2` for runtime
  errors

## Configuration

Required: `./config.yaml` or path via `WI_CONFIG` env / `--config` flag.

Relative `vault.root` is resolved against the config file's parent dir,
not the process CWD — so installed `wi` always finds the right vault.

Template resolution order:
1. `--template-dir` / `WI_TEMPLATE_DIR`
2. `<vault>/.wiki-ingest/templates/` (per-vault override)
3. `$XDG_DATA_HOME/wi-ingest/templates/` (installer default)
4. `./templates/` (development)

## Credentials

External APIs require credentials. Set via env vars (recommended) or
`.wiki-ingest/credentials.json` in the vault:

| Source | Env vars |
|--------|---------|
| Google (Gmail/Drive/Calendar) | `WI_GOOGLE_CLIENT_ID`, `WI_GOOGLE_CLIENT_SECRET`, `WI_GOOGLE_REFRESH_TOKEN` |
| Slack | `WI_SLACK_TOKEN` |
| Jira | `WI_JIRA_URL`, `WI_JIRA_EMAIL`, `WI_JIRA_TOKEN` |
| LLM (optional) | `ANTHROPIC_API_KEY` |

Missing LLM key falls back to `NoopLlmClient` — ingest still runs but
without summarization or concept extraction.

## Atomic ingest flow

`wi ingest` runs in four phases:

1. **Plan** — fetch from all sources, normalize, dedup-check, classify
2. **Write daily + concept pages** — atomic per-file (tmp + rename)
3. **Write work-log** — aggregated personal events across sources
4. **Commit dedup** — only if every write succeeded

If any write fails, dedup is NOT committed and the next run reprocesses
those events. There is no partial-success state from the user's POV.

## When NOT to invoke

- Don't call `wi ingest` from a scheduled context if cron is already
  running it — duplicate runs are safe (dedup blocks) but wasteful
- Don't run `wi maintenance` while `wi ingest` is running (redb
  single-writer file lock conflict — maintenance prints a warning)
- Don't pass `--force` casually — it skips dedup and re-writes pages

## Examples

```bash
# Daily routine (typically via cron)
wi ingest

# Verify config and credentials
wi validate
wi ingest --dry-run

# Backfill last week's data
wi ingest --date 2026-05-20
wi ingest --date 2026-05-21

# Generate weekly synthesis on Monday for the just-completed week
wi synthesis weekly --previous

# Generate crontab for unattended scheduling
wi schedule > /tmp/wi-cron && crontab /tmp/wi-cron
```
