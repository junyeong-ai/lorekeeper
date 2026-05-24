---
name: lorekeeper
description: Daily knowledge ingestion pipeline that collects from Gmail, Google Drive, Google Calendar, Slack, and Jira into an Obsidian vault. Deduplicates events, classifies them with configurable labels and personal-work flags, extracts concepts via Claude, and writes structured markdown pages. Tracks personal work for weekly/monthly/quarterly/annual performance reviews. Atomic 5-phase ingest guarantees no data loss on partial failures. Read-only commands (status, health, performance) are safe to invoke any time; ingest and synthesis are typically cron-scheduled but can be triggered ad-hoc.
when_to_use: |
  ingest, 수집, 위키 수집, 정리, 업무 정리, 종합 보고, weekly summary, monthly summary,
  quarterly review, annual review, performance review, 성과 리뷰, 분기 리뷰,
  주간 정리, 월간 정리, 연간 정리, health check, 헬스 체크, 상태 확인,
  schedule generation, cron, 스케줄 생성, ai news, team digest, work log,
  업무 기록, concept extraction, 개념 추출, vault ingest, dedup pruning,
  log rotation, 메일 정리, 캘린더 정리, 슬랙 트렌드, jira 업무, 내 업무 추적
argument-hint: "<subcommand> [args]"
allowed-tools: |
  Bash(lore *)
  Bash(crontab *)
---

# Lorekeeper — Daily knowledge ingestion for Obsidian

Run the `lore` CLI for ad-hoc ingest, synthesis, and operations. The CLI is
config-driven (`./config.yaml` or `LORE_CONFIG`) and writes to an Obsidian
vault. All commands accept `--config <path>` to override.

## Command reference

| Command | Purpose |
|---------|---------|
| `lore validate` | Parse + validate `config.yaml`, print summary |
| `lore ingest [source]` | Collect from one source (or all enabled), write daily/concept pages, aggregate work-log |
| `lore ingest --dry-run` | Preview ingest without vault writes |
| `lore ingest --date YYYY-MM-DD` | Backfill events for a specific date |
| `lore ingest --force` | Skip dedup cache, re-process everything |
| `lore synthesis weekly [--previous]` | Generate weekly synthesis page + personal weekly summary |
| `lore synthesis monthly` | Aggregate work-log into monthly summary |
| `lore synthesis quarterly` | Quarterly performance review with category stats |
| `lore synthesis annual` | Annual review from quarterly summaries |
| `lore status` | Last successful ingest per source |
| `lore health [--strict]` | Warn if any source is stale (>48h); `--strict` also fails on never-ingested |
| `lore performance` | Work category distribution from recent work-log pages |
| `lore schedule [--bin <path>]` | Print self-contained crontab entries (includes `--config`, `--previous`) |
| `lore maintenance` | Prune ingest log + dedup cache older than 90 days |

## Natural-language trigger mapping

When users ask in Korean, route to the appropriate command:

- "오늘 수집해줘" / "이메일만 수집해줘" → `lore ingest [source]`
- "지난주 정리해줘" / "이번주 업무 정리" → `lore synthesis weekly --previous`
- "지난달 요약" → `lore synthesis monthly --previous`
- "지난 분기 리뷰" → `lore synthesis quarterly --previous`
- "성과 보여줘" / "업무 분포" → `lore performance`
- "상태 확인" → `lore status`
- "헬스 체크" → `lore health`
- "스케줄 등록" / "크론 만들어줘" → `lore schedule` then paste into `crontab -e`
- "오래된 로그 정리" → `lore maintenance`

English equivalents: "ingest today", "weekly summary", "monthly review",
"check status", "show performance", etc.

## Output semantics

- Progress and human-readable diagnostics → **stderr**
- Data (cron lines from `lore schedule`) → **stdout**
- Exit codes: `0` success, `1` for `lore health` when stale sources exist
  (or when `--strict` and any source is never ingested), `2` for runtime
  errors

## Configuration

Required: `./config.yaml` or path via `LORE_CONFIG` env / `--config` flag.

Relative `vault.root` is resolved against the config file's parent dir,
not the process CWD — so installed `lore` always finds the right vault.

Templates are embedded in the binary (no external directory needed). To override,
pass `--template-dir` / `LORE_TEMPLATE_DIR` pointing to a custom template directory.

## Credentials

External APIs require credentials. Set via env vars (recommended) or
`.lorekeeper/credentials.json` in the vault:

| Source | Env vars |
|--------|---------|
| Google (Gmail/Drive/Calendar) | `LORE_GOOGLE_CLIENT_ID`, `LORE_GOOGLE_CLIENT_SECRET`, `LORE_GOOGLE_REFRESH_TOKEN` |
| Slack | `LORE_SLACK_TOKEN` |
| Jira | `LORE_JIRA_URL`, `LORE_JIRA_EMAIL`, `LORE_JIRA_TOKEN` |
| LLM (optional) | `ANTHROPIC_API_KEY` |

Missing LLM key falls back to `NoopLlmClient` — ingest still runs but
without summarization or concept extraction.

## Atomic ingest flow

`lore ingest` runs in five phases:

1. **Plan** — fetch from all sources, normalize, dedup-check, classify
2. **Write daily + concept pages** — atomic per-file (tmp + rename)
3. **Write work-log** — aggregated personal events across sources
4. **Flush LLM queue** — atomic temp+rename of the JSONL task file (queue mode)
5. **Commit dedup** — only if every write AND the flush succeeded

The flush precedes the dedup commit so a crash between them re-extracts
and re-queues on the next run rather than stranding semantic work. If any
write or flush fails, dedup is NOT committed, the process exits non-zero,
and the next run reprocesses those events. No partial-success state from
the user's POV.

## When NOT to invoke

- Don't call `lore ingest` from a scheduled context if cron is already
  running it — duplicate runs are safe (dedup blocks) but wasteful
- Don't run `lore maintenance` while `lore ingest` is running (redb
  single-writer file lock conflict — maintenance prints a warning)
- Don't pass `--force` casually — it skips dedup and re-writes pages

## Examples

```bash
# Daily routine (typically via cron)
lore ingest

# Verify config and credentials
lore validate
lore ingest --dry-run

# Backfill last week's data
lore ingest --date 2026-05-20
lore ingest --date 2026-05-21

# Generate weekly synthesis on Monday for the just-completed week
lore synthesis weekly --previous

# Generate crontab for unattended scheduling
lore schedule > /tmp/lore-cron && crontab /tmp/lore-cron
```
